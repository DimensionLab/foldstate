// SPDX-License-Identifier: Apache-2.0
//! The state of a machine between two events, and the reversible operations that change it.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use serde_json::Value;

use crate::def::Kind;
use crate::error::{EvalError, WireError};
use crate::machine::{Machine, ROOT, StateId};
use crate::value;
use crate::wire::{Invocation, Status, Timer, WireOp, WireSnapshot};

/// The complete state of a machine between two events.
///
/// A snapshot belongs to the machine that made it. Read it through [`Machine`]
/// (`is_in`, `configuration`, `to_wire`) and the accessors here.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub(crate) machine: [u8; 32],
    /// Active states, sorted, root included.
    pub(crate) config: Vec<StateId>,
    pub(crate) history: BTreeMap<StateId, Vec<StateId>>,
    pub(crate) context: Value,
    pub(crate) timers: BTreeMap<String, Timer>,
    pub(crate) invocations: BTreeMap<String, Invocation>,
    pub(crate) status: Status,
}

/// One reversible change to a snapshot.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Op {
    Enter(StateId),
    Exit(StateId),
    Set {
        path: String,
        before: Option<Value>,
        after: Option<Value>,
    },
    History {
        state: StateId,
        before: Option<Vec<StateId>>,
        after: Option<Vec<StateId>>,
    },
    Timer {
        id: String,
        before: Option<Timer>,
        after: Option<Timer>,
    },
    Invocation {
        id: String,
        before: Option<Invocation>,
        after: Option<Invocation>,
    },
    Status {
        before: Status,
        after: Status,
    },
}

fn set_or_remove<V>(map: &mut BTreeMap<String, V>, id: &str, v: &Option<V>)
where
    V: Clone,
{
    match v {
        Some(v) => {
            map.insert(id.to_string(), v.clone());
        }
        None => {
            map.remove(id);
        }
    }
}

impl Snapshot {
    /// The snapshot before a machine is started: nothing active, no context.
    pub(crate) fn blank(machine: [u8; 32]) -> Self {
        Snapshot {
            machine,
            config: Vec::new(),
            history: BTreeMap::new(),
            context: Value::Null,
            timers: BTreeMap::new(),
            invocations: BTreeMap::new(),
            status: Status::Active,
        }
    }

    pub fn context(&self) -> &Value {
        &self.context
    }

    pub fn status(&self) -> Status {
        self.status
    }

    /// True when the machine has finished and accepts no more events.
    pub fn is_done(&self) -> bool {
        self.status == Status::Done
    }

    /// Scheduled events that have not been delivered or cancelled, by timer id.
    pub fn timers(&self) -> &BTreeMap<String, Timer> {
        &self.timers
    }

    /// Work that states have started and still own, by invocation id.
    pub fn invocations(&self) -> &BTreeMap<String, Invocation> {
        &self.invocations
    }

    pub(crate) fn is_active(&self, s: StateId) -> bool {
        self.config.binary_search(&s).is_ok()
    }

    /// The active states numbered `lo ..= hi`.
    pub(crate) fn active_in(&self, lo: StateId, hi: StateId) -> &[StateId] {
        let a = self.config.partition_point(|&x| x < lo);
        let b = self.config.partition_point(|&x| x <= hi);
        &self.config[a..b.max(a)]
    }

    fn forward_or_back(&mut self, op: &Op, forward: bool) -> Result<(), EvalError> {
        match op {
            Op::Enter(s) | Op::Exit(s) => {
                let enter = matches!(op, Op::Enter(_)) == forward;
                match (self.config.binary_search(s), enter) {
                    (Err(i), true) => self.config.insert(i, *s),
                    (Ok(i), false) => {
                        self.config.remove(i);
                    }
                    _ => {}
                }
            }
            Op::Set {
                path,
                before,
                after,
            } => {
                let v = if forward { after } else { before };
                value::put(&mut self.context, path, v.clone())?;
            }
            Op::History {
                state,
                before,
                after,
            } => match if forward { after } else { before } {
                Some(v) => {
                    self.history.insert(*state, v.clone());
                }
                None => {
                    self.history.remove(state);
                }
            },
            Op::Timer { id, before, after } => {
                set_or_remove(&mut self.timers, id, if forward { after } else { before });
            }
            Op::Invocation { id, before, after } => {
                set_or_remove(
                    &mut self.invocations,
                    id,
                    if forward { after } else { before },
                );
            }
            Op::Status { before, after } => {
                self.status = if forward { *after } else { *before };
            }
        }
        Ok(())
    }

    pub(crate) fn apply(&mut self, op: &Op) -> Result<(), EvalError> {
        self.forward_or_back(op, true)
    }

    pub(crate) fn revert(&mut self, op: &Op) -> Result<(), EvalError> {
        self.forward_or_back(op, false)
    }

    pub(crate) fn apply_all(&mut self, ops: &[Op]) -> Result<(), EvalError> {
        ops.iter().try_for_each(|op| self.apply(op))
    }

    pub(crate) fn revert_all(&mut self, ops: &[Op]) -> Result<(), EvalError> {
        ops.iter().rev().try_for_each(|op| self.revert(op))
    }
}

impl Machine {
    fn path(&self, s: StateId) -> String {
        self.state(s).path.clone()
    }

    fn paths(&self, v: &[StateId]) -> Vec<String> {
        v.iter().map(|&s| self.path(s)).collect()
    }

    fn id_of(&self, path: &str) -> Result<StateId, WireError> {
        self.state_id(path)
            .ok_or_else(|| WireError::UnknownState(path.to_string()))
    }

    fn ids_of(&self, paths: &[String]) -> Result<Vec<StateId>, WireError> {
        paths.iter().map(|p| self.id_of(p)).collect()
    }

    /// Whether the state with this path is active. The root (`""`) always is.
    pub fn is_in(&self, snapshot: &Snapshot, path: &str) -> bool {
        self.state_id(path).is_some_and(|s| snapshot.is_active(s))
    }

    /// Paths of the active states in document order, without the root.
    pub fn configuration<'a>(&'a self, snapshot: &Snapshot) -> Vec<&'a str> {
        snapshot
            .config
            .iter()
            .filter(|&&s| s != ROOT)
            .map(|&s| self.state(s).path.as_str())
            .collect()
    }

    /// Paths of the active states that have no active child.
    pub fn leaves<'a>(&'a self, snapshot: &Snapshot) -> Vec<&'a str> {
        snapshot
            .config
            .iter()
            .filter(|&&s| self.is_atomic(s))
            .map(|&s| self.state(s).path.as_str())
            .collect()
    }

    /// Whether some active state carries the tag.
    pub fn has_tag(&self, snapshot: &Snapshot, tag: &str) -> bool {
        snapshot
            .config
            .iter()
            .any(|&s| self.state(s).tags.iter().any(|t| t == tag))
    }

    /// The serialised form of a snapshot.
    pub fn to_wire(&self, snapshot: &Snapshot) -> WireSnapshot {
        WireSnapshot {
            schema: crate::wire::SNAPSHOT_SCHEMA.into(),
            machine: self.hash_hex.clone(),
            status: snapshot.status,
            configuration: self
                .configuration(snapshot)
                .into_iter()
                .map(String::from)
                .collect(),
            history: snapshot
                .history
                .iter()
                .map(|(h, v)| (self.path(*h), self.paths(v)))
                .collect(),
            context: snapshot.context.clone(),
            timers: snapshot.timers.clone(),
            invocations: snapshot.invocations.clone(),
        }
    }

    /// Reads a serialised snapshot, checking that it was made by this definition and that
    /// its configuration is legal.
    pub fn from_wire(&self, wire: &WireSnapshot) -> Result<Snapshot, WireError> {
        if wire.schema != crate::wire::SNAPSHOT_SCHEMA {
            return Err(WireError::UnknownSchema(wire.schema.clone()));
        }
        if wire.machine != self.hash_hex {
            return Err(WireError::MachineMismatch);
        }
        let mut config = self.ids_of(&wire.configuration)?;
        config.push(ROOT);
        config.sort_unstable();
        config.dedup();
        let mut history = BTreeMap::new();
        for (h, v) in &wire.history {
            let hid = self.id_of(h)?;
            if self.state(hid).kind != Kind::History {
                return Err(WireError::IllegalConfiguration(alloc::format!(
                    "`{h}` is not a history state"
                )));
            }
            history.insert(hid, self.ids_of(v)?);
        }
        let snapshot = Snapshot {
            machine: self.hash,
            config,
            history,
            context: wire.context.clone(),
            timers: wire.timers.clone(),
            invocations: wire.invocations.clone(),
            status: wire.status,
        };
        self.check_configuration(&snapshot)?;
        Ok(snapshot)
    }

    /// A configuration is legal when the root is active, every active state's parent is
    /// active, an active compound state has exactly one active child and an active parallel
    /// state has all of them, and no history state is active.
    pub(crate) fn check_configuration(&self, snapshot: &Snapshot) -> Result<(), WireError> {
        let bad = |why: &str, s: StateId| {
            Err(WireError::IllegalConfiguration(alloc::format!(
                "{why}: `{}`",
                self.state(s).path
            )))
        };
        if !snapshot.is_active(ROOT) {
            return bad("the root is not active", ROOT);
        }
        for &s in &snapshot.config {
            let st = self.state(s);
            if let Some(p) = st.parent
                && !snapshot.is_active(p)
            {
                return bad("active state with an inactive parent", s);
            }
            let active = st
                .children
                .iter()
                .filter(|c| snapshot.is_active(**c))
                .count();
            match st.kind {
                Kind::Compound if active != 1 => {
                    return bad("compound state without exactly one active child", s);
                }
                Kind::Parallel if active != st.children.len() => {
                    return bad("parallel state with an inactive region", s);
                }
                Kind::History => return bad("active history state", s),
                _ => {}
            }
        }
        Ok(())
    }

    pub(crate) fn op_to_wire(&self, op: &Op) -> WireOp {
        match op {
            Op::Enter(s) => WireOp::Enter(self.path(*s)),
            Op::Exit(s) => WireOp::Exit(self.path(*s)),
            Op::Set {
                path,
                before,
                after,
            } => WireOp::Set {
                path: path.clone(),
                before: before.clone(),
                after: after.clone(),
            },
            Op::History {
                state,
                before,
                after,
            } => WireOp::History {
                state: self.path(*state),
                before: before.as_deref().map(|v| self.paths(v)),
                after: after.as_deref().map(|v| self.paths(v)),
            },
            Op::Timer { id, before, after } => WireOp::Timer {
                id: id.clone(),
                before: before.clone(),
                after: after.clone(),
            },
            Op::Invocation { id, before, after } => WireOp::Invocation {
                id: id.clone(),
                before: before.clone(),
                after: after.clone(),
            },
            Op::Status { before, after } => WireOp::Status {
                before: *before,
                after: *after,
            },
        }
    }

    pub(crate) fn op_from_wire(&self, op: &WireOp) -> Result<Op, WireError> {
        let list = |v: &Option<Vec<String>>| v.as_deref().map(|p| self.ids_of(p)).transpose();
        Ok(match op {
            WireOp::Enter(state) => Op::Enter(self.id_of(state)?),
            WireOp::Exit(state) => Op::Exit(self.id_of(state)?),
            WireOp::Set {
                path,
                before,
                after,
            } => Op::Set {
                path: path.clone(),
                before: before.clone(),
                after: after.clone(),
            },
            WireOp::History {
                state,
                before,
                after,
            } => Op::History {
                state: self.id_of(state)?,
                before: list(before)?,
                after: list(after)?,
            },
            WireOp::Timer { id, before, after } => Op::Timer {
                id: id.clone(),
                before: before.clone(),
                after: after.clone(),
            },
            WireOp::Invocation { id, before, after } => Op::Invocation {
                id: id.clone(),
                before: before.clone(),
                after: after.clone(),
            },
            WireOp::Status { before, after } => Op::Status {
                before: *before,
                after: *after,
            },
        })
    }
}

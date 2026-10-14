// SPDX-License-Identifier: Apache-2.0
//! Serialised forms: what crosses a process boundary or is stored. State references are
//! paths, so these forms do not depend on how a definition was compiled.
//!
//! JSON Schemas: `docs/schema/snapshot.schema.json`, `docs/schema/entry.schema.json`.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::def::{Effect, Event};

/// Identifier of the serialised snapshot format.
pub const SNAPSHOT_SCHEMA: &str = "foldstate-snapshot/1";

fn snapshot_schema() -> String {
    SNAPSHOT_SCHEMA.into()
}

/// Whether a machine still accepts events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Active,
    /// A final configuration was reached.
    Done,
}

impl Status {
    fn is_active(&self) -> bool {
        matches!(self, Status::Active)
    }
}

/// A scheduled event that has not been delivered or cancelled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Timer {
    pub event: String,
    pub delay_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_ms: Option<i64>,
}

/// Work a state started and still owns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Invocation {
    /// Path of the owning state.
    pub state: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
}

/// The complete state of a machine between two events.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireSnapshot {
    /// Always [`SNAPSHOT_SCHEMA`].
    #[serde(default = "snapshot_schema")]
    pub schema: String,
    /// SHA-256 (hex) of the canonical JSON of the definition.
    pub machine: String,
    #[serde(default, skip_serializing_if = "Status::is_active")]
    pub status: Status,
    /// Paths of the active states in document order. The root is always active and not listed.
    pub configuration: Vec<String>,
    /// Remembered configurations, by path of the history state.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub history: BTreeMap<String, Vec<String>>,
    pub context: Value,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub timers: BTreeMap<String, Timer>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub invocations: BTreeMap<String, Invocation>,
}

/// Why a serialised entry does not apply to a serialised snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayError {
    /// Sequence number of the entry.
    pub seq: u64,
}

impl core::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "entry {} does not apply to this snapshot", self.seq)
    }
}

impl core::error::Error for ReplayError {}

fn set_or_remove<V: Clone>(map: &mut BTreeMap<String, V>, id: &str, v: &Option<V>) {
    match v {
        Some(v) => {
            map.insert(id.into(), v.clone());
        }
        None => {
            map.remove(id);
        }
    }
}

impl WireSnapshot {
    /// The snapshot before the first entry of a log.
    pub fn blank(machine: impl Into<String>) -> Self {
        WireSnapshot {
            schema: snapshot_schema(),
            machine: machine.into(),
            ..WireSnapshot::default()
        }
    }

    /// Applies an entry's changes, without the definition: enough to follow a log entry by
    /// entry and show which states are active. `configuration` is kept sorted by path
    /// (document order needs the definition; use [`crate::Log`] when you have it).
    pub fn redo(&mut self, entry: &WireEntry) -> Result<(), ReplayError> {
        for op in &entry.ops {
            self.one(op, true)
                .map_err(|_| ReplayError { seq: entry.seq })?;
        }
        Ok(())
    }

    /// Takes an entry's changes back out.
    pub fn undo(&mut self, entry: &WireEntry) -> Result<(), ReplayError> {
        for op in entry.ops.iter().rev() {
            self.one(op, false)
                .map_err(|_| ReplayError { seq: entry.seq })?;
        }
        Ok(())
    }

    fn one(&mut self, op: &WireOp, forward: bool) -> Result<(), crate::error::EvalError> {
        match op {
            // The root is always active and never listed.
            WireOp::Enter(state) | WireOp::Exit(state) if state.is_empty() => {}
            WireOp::Enter(state) | WireOp::Exit(state) => {
                let enter = matches!(op, WireOp::Enter(_)) == forward;
                match (self.configuration.binary_search(state), enter) {
                    (Err(i), true) => self.configuration.insert(i, state.clone()),
                    (Ok(i), false) => {
                        self.configuration.remove(i);
                    }
                    _ => {}
                }
            }
            WireOp::Set {
                path,
                before,
                after,
            } => {
                let v = if forward { after } else { before };
                crate::value::put(&mut self.context, path, v.clone())?;
            }
            WireOp::History {
                state,
                before,
                after,
            } => set_or_remove(
                &mut self.history,
                state,
                if forward { after } else { before },
            ),
            WireOp::Timer { id, before, after } => {
                set_or_remove(&mut self.timers, id, if forward { after } else { before })
            }
            WireOp::Invocation { id, before, after } => set_or_remove(
                &mut self.invocations,
                id,
                if forward { after } else { before },
            ),
            WireOp::Status { before, after } => {
                self.status = if forward { *after } else { *before };
            }
        }
        Ok(())
    }
}

/// One reversible change to a snapshot. An entry's `ops`, applied in order, turn the snapshot
/// before the entry into the snapshot after it; inverted and applied in reverse, the opposite.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireOp {
    /// The state with this path became active.
    Enter(String),
    /// The state with this path stopped being active.
    Exit(String),
    /// Replaces, adds (`before` absent) or removes (`after` absent) a context value.
    Set {
        path: String,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "crate::value::present"
        )]
        before: Option<Value>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "crate::value::present"
        )]
        after: Option<Value>,
    },
    History {
        state: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<Vec<String>>,
    },
    Timer {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before: Option<Timer>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<Timer>,
    },
    Invocation {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before: Option<Invocation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        after: Option<Invocation>,
    },
    Status {
        before: Status,
        after: Status,
    },
}

/// A transition, identified by its source state and its position in that state's list
/// (`on` first, then `after`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransitionRef {
    pub source: String,
    pub index: u32,
}

/// What an entry records.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireEntryKind {
    /// The machine was started.
    Start {
        #[serde(default, skip_serializing_if = "Value::is_null")]
        input: Value,
    },
    /// An event was processed.
    Event(Event),
    /// The state of an earlier entry was made current again.
    Restore { to: u64 },
}

/// One entry of a log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireEntry {
    /// Position on its branch, starting at 1. A fork continues its parent's numbering.
    pub seq: u64,
    /// `{"start": {"input": …}}`, `{"event": {…}}` or `{"restore": {"to": n}}`.
    pub kind: WireEntryKind,
    /// False when the event enabled no transition and was ignored.
    #[serde(default = "yes", skip_serializing_if = "is_true")]
    pub handled: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ops: Vec<WireOp>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<Effect>,
    /// The transitions taken, one list per microstep.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub microsteps: Vec<Vec<TransitionRef>>,
    /// Results of host-supplied guards, in evaluation order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub guards: Vec<GuardResult>,
}

fn yes() -> bool {
    true
}

fn is_true(b: &bool) -> bool {
    *b
}

/// The result of one evaluation of a host-supplied guard.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuardResult {
    pub name: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub params: Value,
    /// Absent when the host does not know the guard; it then counts as false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<bool>,
}

/// A branch of a serialised log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireBranch {
    pub id: u32,
    /// The branch and entry this one was forked from. Absent for the first branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<WireParent>,
    pub entries: Vec<WireEntry>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireParent {
    pub branch: u32,
    pub seq: u64,
}

/// A serialised log: a tree of branches.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireLog {
    /// Always `foldstate-log/1`.
    pub schema: String,
    /// SHA-256 (hex) of the canonical JSON of the definition.
    pub machine: String,
    pub branches: Vec<WireBranch>,
}

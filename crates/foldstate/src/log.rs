// SPDX-License-Identifier: Apache-2.0
//! The log: an append-only tree of entries. The state at any entry is the fold of the
//! entries before it; nothing is ever changed or removed.

use alloc::string::String;
use alloc::vec::Vec;
use core::cell::Cell;
use serde::Serialize;
use serde_json::Value;

use crate::def::{Effect, Event};
use crate::error::{LogError, WireError};
use crate::kernel::Change;
use crate::machine::{Machine, ROOT};
use crate::snapshot::{Op, Snapshot};
use crate::value::{self, ValueChange};
use crate::wire::{GuardResult, Status, WireBranch, WireEntry, WireEntryKind, WireLog, WireParent};
use crate::{Guards, NoGuards};

/// Identifier of the serialised log format.
pub const LOG_SCHEMA: &str = "foldstate-log/1";

/// A branch of a log. The first branch is [`MAIN`].
pub type BranchId = u32;

/// The branch a log starts with.
pub const MAIN: BranchId = 0;

/// What an entry records.
#[derive(Debug, Clone, PartialEq)]
pub enum EntryKind {
    /// The machine was started with this input.
    Start { input: Value },
    /// An event was processed.
    Event(Event),
    /// The state of an earlier entry was made current again.
    Restore { to: u64 },
}

/// One entry of a log: what happened and what it changed.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// Position on its branch, starting at 1. A fork continues its parent's numbering.
    pub seq: u64,
    pub kind: EntryKind,
    pub change: Change,
}

impl Entry {
    /// The event, for entries that record one.
    pub fn event(&self) -> Option<&Event> {
        match &self.kind {
            EntryKind::Event(e) => Some(e),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
struct Branch {
    parent: Option<(BranchId, u64)>,
    /// Sequence number of `entries[0]`.
    first: u64,
    entries: Vec<Entry>,
    /// Snapshots after some entries of this branch, ascending by sequence number.
    checkpoints: Vec<(u64, Snapshot)>,
    head: Snapshot,
}

impl Branch {
    fn head_seq(&self) -> u64 {
        self.first + self.entries.len() as u64 - 1
    }
}

/// Where a branch comes from and how far it goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct BranchInfo {
    pub id: BranchId,
    /// The branch and entry it was forked from.
    pub parent: Option<(BranchId, u64)>,
    /// Sequence number of its first own entry.
    pub first: u64,
    /// Sequence number of its last entry. Less than `first` for a fork with no entry yet.
    pub head: u64,
}

/// The difference between two positions of a log.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Diff {
    /// The last entry both positions share; 0 when they share none.
    pub shared: u64,
    /// States active only at the first position, in document order.
    pub only_left: Vec<String>,
    /// States active only at the second position, in document order.
    pub only_right: Vec<String>,
    pub context: Vec<ValueChange>,
    /// Ids of timers that are pending on one side only, or differ.
    pub timers: Vec<String>,
    /// Ids of invocations that are started on one side only, or differ.
    pub invocations: Vec<String>,
    /// The two statuses, when they differ.
    pub status: Option<(Status, Status)>,
}

impl Diff {
    /// True when the two positions are in the same state.
    pub fn is_empty(&self) -> bool {
        self.only_left.is_empty()
            && self.only_right.is_empty()
            && self.context.is_empty()
            && self.timers.is_empty()
            && self.invocations.is_empty()
            && self.status.is_none()
    }
}

/// An append-only tree of entries for one machine.
#[derive(Debug, Clone)]
pub struct Log {
    machine: [u8; 32],
    machine_hex: String,
    branches: Vec<Branch>,
    checkpoint_every: u64,
}

/// Host guards answered from the results recorded in an entry.
struct Recorded<'a> {
    results: &'a [GuardResult],
    next: Cell<usize>,
}

impl Guards for Recorded<'_> {
    fn eval(&self, name: &str, _: &Value, _: &Value, _: &Event) -> Option<bool> {
        let i = self.next.get();
        self.next.set(i + 1);
        self.results
            .get(i)
            .filter(|r| r.name == name)
            .and_then(|r| r.result)
    }
}

impl Log {
    /// Starts a machine and a log for it. The effects of starting are in entry 1.
    pub fn start(machine: &Machine, input: Value) -> Result<Log, LogError> {
        Log::start_with(machine, input, &NoGuards)
    }

    /// [`Log::start`] with host guards.
    pub fn start_with(
        machine: &Machine,
        input: Value,
        guards: &dyn Guards,
    ) -> Result<Log, LogError> {
        let step = machine.start_with(input.clone(), guards)?;
        Ok(Log {
            machine: machine.hash,
            machine_hex: machine.hash_hex.clone(),
            branches: alloc::vec![Branch {
                parent: None,
                first: 1,
                entries: alloc::vec![Entry {
                    seq: 1,
                    kind: EntryKind::Start { input },
                    change: step.change,
                }],
                checkpoints: Vec::new(),
                head: step.snapshot,
            }],
            checkpoint_every: 256,
        })
    }

    /// Keeps a snapshot every `n` entries (default 256). Smaller is faster to read at an
    /// arbitrary entry and uses more memory. Applies to entries appended from now on.
    pub fn set_checkpoint_interval(&mut self, n: u64) {
        self.checkpoint_every = n.max(1);
    }

    /// SHA-256 (hex) of the definition this log belongs to.
    pub fn machine(&self) -> &str {
        &self.machine_hex
    }

    fn branch(&self, id: BranchId) -> Result<&Branch, LogError> {
        self.branches
            .get(id as usize)
            .ok_or(LogError::UnknownBranch(id))
    }

    fn check(&self, machine: &Machine) -> Result<(), LogError> {
        if machine.hash == self.machine {
            Ok(())
        } else {
            Err(LogError::MachineMismatch)
        }
    }

    /// All branches, in order of creation.
    pub fn branches(&self) -> impl Iterator<Item = BranchInfo> + '_ {
        self.branches.iter().enumerate().map(|(i, b)| BranchInfo {
            id: i as BranchId,
            parent: b.parent,
            first: b.first,
            head: b.head_seq(),
        })
    }

    /// The current snapshot of a branch.
    pub fn head(&self, branch: BranchId) -> Result<&Snapshot, LogError> {
        Ok(&self.branch(branch)?.head)
    }

    /// Sequence number of the last entry of a branch.
    pub fn head_seq(&self, branch: BranchId) -> Result<u64, LogError> {
        Ok(self.branch(branch)?.head_seq())
    }

    /// The entry `seq` as seen from `branch`: its own, or the one it inherited.
    pub fn entry(&self, branch: BranchId, seq: u64) -> Result<&Entry, LogError> {
        let unknown = LogError::UnknownSeq { branch, seq };
        let mut b = self.branch(branch)?;
        if seq == 0 || seq > b.head_seq() {
            return Err(unknown);
        }
        loop {
            if seq >= b.first {
                return b.entries.get((seq - b.first) as usize).ok_or(unknown);
            }
            match b.parent {
                Some((p, _)) => b = self.branch(p)?,
                None => return Err(unknown),
            }
        }
    }

    /// The entries `from ..= to` as seen from `branch`.
    pub fn entries(&self, branch: BranchId, from: u64, to: u64) -> Result<Vec<&Entry>, LogError> {
        (from..=to).map(|s| self.entry(branch, s)).collect()
    }

    fn appended(&mut self, branch: BranchId, kind: EntryKind, change: Change) -> &Entry {
        let every = self.checkpoint_every;
        let b = &mut self.branches[branch as usize];
        let seq = b.head_seq() + 1;
        if seq.is_multiple_of(every) {
            b.checkpoints.push((seq, b.head.clone()));
        }
        b.entries.push(Entry { seq, kind, change });
        &b.entries[b.entries.len() - 1]
    }

    /// Processes an event on a branch and records it. A refused event is not recorded.
    pub fn append(
        &mut self,
        machine: &Machine,
        branch: BranchId,
        event: Event,
    ) -> Result<&Entry, LogError> {
        self.append_with(machine, branch, event, &NoGuards)
    }

    /// [`Log::append`] with host guards.
    pub fn append_with(
        &mut self,
        machine: &Machine,
        branch: BranchId,
        event: Event,
        guards: &dyn Guards,
    ) -> Result<&Entry, LogError> {
        self.check(machine)?;
        self.branch(branch)?;
        let b = &mut self.branches[branch as usize];
        let change = machine.step_in_place(&mut b.head, &event, guards)?;
        Ok(self.appended(branch, EntryKind::Event(event), change))
    }

    /// The snapshot after entry `seq`, as seen from `branch`.
    pub fn state_at(&self, branch: BranchId, seq: u64) -> Result<Snapshot, LogError> {
        let target = self.branch(branch)?;
        if seq == 0 || seq > target.head_seq() {
            return Err(LogError::UnknownSeq { branch, seq });
        }
        if seq == target.head_seq() {
            return Ok(target.head.clone());
        }
        // The nearest checkpoint at or before `seq` on the way back to the start.
        let mut snapshot = Snapshot::blank(self.machine);
        let mut from = 0u64;
        let mut b = target;
        let mut limit = seq;
        loop {
            let i = b.checkpoints.partition_point(|(s, _)| *s <= limit);
            if i > 0 {
                let (s, snap) = &b.checkpoints[i - 1];
                snapshot = snap.clone();
                from = *s;
                break;
            }
            match b.parent {
                Some((p, at)) => {
                    limit = limit.min(at);
                    b = self.branch(p)?;
                }
                None => break,
            }
        }
        for s in from + 1..=seq {
            self.redo(&mut snapshot, self.entry(branch, s)?)?;
        }
        Ok(snapshot)
    }

    fn redo(&self, snapshot: &mut Snapshot, entry: &Entry) -> Result<(), LogError> {
        entry
            .change
            .redo(snapshot)
            .map_err(|_| LogError::Malformed("an entry does not apply to the state before it"))
    }

    /// Creates a branch that continues from entry `seq` of `branch`. Entries up to `seq` are
    /// shared with the parent; the new branch's next entry is `seq + 1`.
    pub fn fork(&mut self, branch: BranchId, seq: u64) -> Result<BranchId, LogError> {
        let head = self.state_at(branch, seq)?;
        self.branches.push(Branch {
            parent: Some((branch, seq)),
            first: seq + 1,
            entries: Vec::new(),
            checkpoints: alloc::vec![(seq, head.clone())],
            head,
        });
        Ok((self.branches.len() - 1) as BranchId)
    }

    /// Appends an entry that makes the state after entry `to` current again. History keeps
    /// everything in between. The entry's effects tell the host which timers and invocations
    /// to cancel, stop, schedule and start; no exit, entry or transition action runs.
    pub fn restore(
        &mut self,
        machine: &Machine,
        branch: BranchId,
        to: u64,
    ) -> Result<&Entry, LogError> {
        self.check(machine)?;
        let target = self.state_at(branch, to)?;
        let b = &mut self.branches[branch as usize];
        let change = restore_change(&b.head, &target);
        b.head = target;
        Ok(self.appended(branch, EntryKind::Restore { to }, change))
    }

    /// Recomputes every entry from the definition, the recorded events and the recorded
    /// results of host guards, and compares. The first difference is an error naming its
    /// branch and entry.
    pub fn verify(&self, machine: &Machine) -> Result<(), LogError> {
        self.verify_inner(machine, None)
    }

    /// [`Log::verify`], asking the host's guards again instead of using recorded results.
    pub fn verify_with(&self, machine: &Machine, guards: &dyn Guards) -> Result<(), LogError> {
        self.verify_inner(machine, Some(guards))
    }

    fn verify_inner(&self, machine: &Machine, host: Option<&dyn Guards>) -> Result<(), LogError> {
        self.check(machine)?;
        for (id, b) in self.branches.iter().enumerate() {
            let branch = id as BranchId;
            let mut snapshot = match b.parent {
                Some((p, at)) => self.state_at(p, at)?,
                None => Snapshot::blank(self.machine),
            };
            for entry in &b.entries {
                let seq = entry.seq;
                let diverged = |what| LogError::Diverged { branch, seq, what };
                let recorded = Recorded {
                    results: &entry.change.guards,
                    next: Cell::new(0),
                };
                let guards: &dyn Guards = match host {
                    Some(g) => g,
                    None => &recorded,
                };
                let change = match &entry.kind {
                    EntryKind::Start { input } => {
                        if b.parent.is_some() || seq != 1 {
                            return Err(LogError::Malformed("start is not the first entry"));
                        }
                        let step = machine
                            .start_with(input.clone(), guards)
                            .map_err(|_| diverged("outcomes"))?;
                        snapshot = step.snapshot;
                        step.change
                    }
                    EntryKind::Event(event) => machine
                        .step_in_place(&mut snapshot, event, guards)
                        .map_err(|_| diverged("outcomes"))?,
                    EntryKind::Restore { to } => {
                        let target = self.state_at(branch, *to)?;
                        let change = restore_change(&snapshot, &target);
                        snapshot = target;
                        change
                    }
                };
                if change.handled != entry.change.handled {
                    return Err(diverged("outcomes"));
                }
                if change.ops != entry.change.ops {
                    return Err(diverged("changes"));
                }
                if change.effects != entry.change.effects {
                    return Err(diverged("effects"));
                }
                if change.microsteps != entry.change.microsteps {
                    return Err(diverged("transitions"));
                }
            }
            if snapshot != b.head {
                return Err(LogError::Diverged {
                    branch,
                    seq: b.head_seq(),
                    what: "snapshots",
                });
            }
        }
        Ok(())
    }

    /// A position in the log that can be moved one entry at a time in either direction.
    pub fn cursor(&self, branch: BranchId, seq: u64) -> Result<Cursor<'_>, LogError> {
        Ok(Cursor {
            snapshot: self.state_at(branch, seq)?,
            log: self,
            branch,
            seq,
        })
    }

    /// The difference between the states at two positions, and how much history they share.
    pub fn diff(
        &self,
        machine: &Machine,
        left: (BranchId, u64),
        right: (BranchId, u64),
    ) -> Result<Diff, LogError> {
        self.check(machine)?;
        let a = self.state_at(left.0, left.1)?;
        let b = self.state_at(right.0, right.1)?;
        // Shared entries form a prefix: find its length.
        let (mut lo, mut hi) = (0u64, left.1.min(right.1));
        while lo < hi {
            let mid = lo + (hi - lo).div_ceil(2);
            let same = core::ptr::eq(self.entry(left.0, mid)?, self.entry(right.0, mid)?);
            if same {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        let paths = |x: &Snapshot, y: &Snapshot| -> Vec<String> {
            x.config
                .iter()
                .filter(|s| **s != ROOT && !y.is_active(**s))
                .map(|&s| machine.state(s).path.clone())
                .collect()
        };
        fn keys<V: PartialEq>(
            x: &alloc::collections::BTreeMap<String, V>,
            y: &alloc::collections::BTreeMap<String, V>,
        ) -> Vec<String> {
            let mut out: Vec<String> = x
                .iter()
                .filter(|(k, v)| y.get(*k) != Some(v))
                .map(|(k, _)| k.clone())
                .chain(y.keys().filter(|k| !x.contains_key(*k)).cloned())
                .collect();
            out.sort();
            out
        }
        Ok(Diff {
            shared: lo,
            only_left: paths(&a, &b),
            only_right: paths(&b, &a),
            context: value::diff(&a.context, &b.context),
            timers: keys(&a.timers, &b.timers),
            invocations: keys(&a.invocations, &b.invocations),
            status: (a.status != b.status).then_some((a.status, b.status)),
        })
    }

    /// The serialised form of the whole log.
    pub fn to_wire(&self, machine: &Machine) -> Result<WireLog, LogError> {
        self.check(machine)?;
        Ok(WireLog {
            schema: LOG_SCHEMA.into(),
            machine: self.machine_hex.clone(),
            branches: self
                .branches
                .iter()
                .enumerate()
                .map(|(i, b)| WireBranch {
                    id: i as BranchId,
                    parent: b.parent.map(|(branch, seq)| WireParent { branch, seq }),
                    entries: b.entries.iter().map(|e| machine.entry_to_wire(e)).collect(),
                })
                .collect(),
        })
    }

    /// Reads a serialised log. Entries are applied, not recomputed: call [`Log::verify`] to
    /// check that the log is what this machine would have written.
    pub fn from_wire(machine: &Machine, wire: &WireLog) -> Result<Log, LogError> {
        if wire.schema != LOG_SCHEMA {
            return Err(LogError::Malformed("unknown log schema"));
        }
        if wire.machine != machine.hash_hex {
            return Err(LogError::MachineMismatch);
        }
        let mut log = Log {
            machine: machine.hash,
            machine_hex: machine.hash_hex.clone(),
            branches: Vec::new(),
            checkpoint_every: 256,
        };
        for (i, wb) in wire.branches.iter().enumerate() {
            if wb.id as usize != i {
                return Err(LogError::Malformed("branches are not numbered in order"));
            }
            let (head, first, parent) = match wb.parent {
                Some(p) => {
                    if p.branch as usize >= i {
                        return Err(LogError::Malformed("a branch comes before its parent"));
                    }
                    (
                        log.state_at(p.branch, p.seq)?,
                        p.seq + 1,
                        Some((p.branch, p.seq)),
                    )
                }
                None => {
                    if i != 0 {
                        return Err(LogError::Malformed("more than one branch without a parent"));
                    }
                    (Snapshot::blank(machine.hash), 1, None)
                }
            };
            let checkpoints = match parent {
                Some((_, seq)) => alloc::vec![(seq, head.clone())],
                None => Vec::new(),
            };
            log.branches.push(Branch {
                parent,
                first,
                entries: Vec::new(),
                checkpoints,
                head,
            });
            for (n, we) in wb.entries.iter().enumerate() {
                if we.seq != first + n as u64 {
                    return Err(LogError::Malformed("sequence numbers are not consecutive"));
                }
                let entry = machine.entry_from_wire(we)?;
                if matches!(entry.kind, EntryKind::Start { .. }) != (parent.is_none() && n == 0) {
                    return Err(LogError::Malformed("start is not the first entry"));
                }
                let b = &mut log.branches[i];
                entry.change.redo(&mut b.head).map_err(|_| {
                    LogError::Malformed("an entry does not apply to the state before it")
                })?;
                let Entry { kind, change, .. } = entry;
                log.appended(i as BranchId, kind, change);
            }
            if parent.is_none() && log.branches[i].entries.is_empty() {
                return Err(LogError::Malformed("the first branch has no start entry"));
            }
            if !log.branches[i].entries.is_empty() {
                machine.check_configuration(&log.branches[i].head)?;
            }
        }
        if log.branches.is_empty() {
            return Err(LogError::Malformed("no branches"));
        }
        Ok(log)
    }
}

/// The change that turns `current` into `target`, with the effects that bring the host's
/// timers and invocations in line.
fn restore_change(current: &Snapshot, target: &Snapshot) -> Change {
    let mut change = Change {
        handled: true,
        ..Change::default()
    };
    if current.status != target.status {
        change.ops.push(Op::Status {
            before: current.status,
            after: target.status,
        });
    }
    for &s in current.config.iter().rev() {
        if !target.is_active(s) {
            change.ops.push(Op::Exit(s));
        }
    }
    for &s in &target.config {
        if !current.is_active(s) {
            change.ops.push(Op::Enter(s));
        }
    }
    for c in value::diff(&current.context, &target.context) {
        change.ops.push(Op::Set {
            path: c.path,
            before: c.before,
            after: c.after,
        });
    }
    let histories: alloc::collections::BTreeSet<_> = current
        .history
        .keys()
        .chain(target.history.keys())
        .collect();
    for &h in histories {
        let (before, after) = (current.history.get(&h), target.history.get(&h));
        if before != after {
            change.ops.push(Op::History {
                state: h,
                before: before.cloned(),
                after: after.cloned(),
            });
        }
    }
    // Stop what is no longer wanted, then start what is wanted again.
    for (id, t) in &current.timers {
        if target.timers.get(id) != Some(t) {
            change.effects.push(Effect::Cancel { id: id.clone() });
            if !target.timers.contains_key(id) {
                change.ops.push(Op::Timer {
                    id: id.clone(),
                    before: Some(t.clone()),
                    after: None,
                });
            }
        }
    }
    for (id, v) in &current.invocations {
        if target.invocations.get(id) != Some(v) {
            change.effects.push(Effect::Stop { id: id.clone() });
            if !target.invocations.contains_key(id) {
                change.ops.push(Op::Invocation {
                    id: id.clone(),
                    before: Some(v.clone()),
                    after: None,
                });
            }
        }
    }
    for (id, t) in &target.timers {
        let before = current.timers.get(id);
        if before != Some(t) {
            change.effects.push(Effect::Schedule {
                id: id.clone(),
                event: t.event.clone(),
                delay_ms: t.delay_ms,
                due_ms: t.due_ms,
            });
            change.ops.push(Op::Timer {
                id: id.clone(),
                before: before.cloned(),
                after: Some(t.clone()),
            });
        }
    }
    for (id, v) in &target.invocations {
        let before = current.invocations.get(id);
        if before != Some(v) {
            change.effects.push(Effect::Start {
                id: id.clone(),
                name: v.name.clone(),
                params: v.params.clone(),
            });
            change.ops.push(Op::Invocation {
                id: id.clone(),
                before: before.cloned(),
                after: Some(v.clone()),
            });
        }
    }
    change
}

/// A position in a log, with the snapshot at that position.
#[derive(Debug, Clone)]
pub struct Cursor<'a> {
    log: &'a Log,
    branch: BranchId,
    seq: u64,
    snapshot: Snapshot,
}

impl<'a> Cursor<'a> {
    /// Sequence number of the entry whose result the cursor shows.
    pub fn seq(&self) -> u64 {
        self.seq
    }

    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }

    /// The entry that produced the current snapshot.
    pub fn entry(&self) -> Result<&'a Entry, LogError> {
        self.log.entry(self.branch, self.seq)
    }

    /// Moves to the next entry and returns it.
    pub fn forward(&mut self) -> Result<&'a Entry, LogError> {
        let entry = self
            .log
            .entry(self.branch, self.seq + 1)
            .map_err(|_| LogError::AtEnd)?;
        self.log.redo(&mut self.snapshot, entry)?;
        self.seq += 1;
        Ok(entry)
    }

    /// Moves to the previous entry and returns the entry that was undone.
    pub fn back(&mut self) -> Result<&'a Entry, LogError> {
        if self.seq <= 1 {
            return Err(LogError::AtEnd);
        }
        let entry = self.log.entry(self.branch, self.seq)?;
        entry
            .change
            .undo(&mut self.snapshot)
            .map_err(|_| LogError::Malformed("an entry does not apply to the state before it"))?;
        self.seq -= 1;
        Ok(entry)
    }

    /// Moves to entry `seq`, one entry at a time when it is near and from a checkpoint
    /// when it is far.
    pub fn seek(&mut self, seq: u64) -> Result<(), LogError> {
        let distance = seq.abs_diff(self.seq);
        if distance > self.log.checkpoint_every {
            self.snapshot = self.log.state_at(self.branch, seq)?;
            self.seq = seq;
            return Ok(());
        }
        if seq == 0 || seq > self.log.head_seq(self.branch)? {
            return Err(LogError::UnknownSeq {
                branch: self.branch,
                seq,
            });
        }
        while self.seq < seq {
            self.forward()?;
        }
        while self.seq > seq {
            self.back()?;
        }
        Ok(())
    }
}

impl Machine {
    /// The serialised form of an entry.
    pub fn entry_to_wire(&self, entry: &Entry) -> WireEntry {
        WireEntry {
            seq: entry.seq,
            kind: match &entry.kind {
                EntryKind::Start { input } => WireEntryKind::Start {
                    input: input.clone(),
                },
                EntryKind::Event(event) => WireEntryKind::Event(event.clone()),
                EntryKind::Restore { to } => WireEntryKind::Restore { to: *to },
            },
            handled: entry.change.handled,
            ops: entry
                .change
                .ops
                .iter()
                .map(|o| self.op_to_wire(o))
                .collect(),
            effects: entry.change.effects.clone(),
            microsteps: entry.change.microsteps(self),
            guards: entry.change.guards.clone(),
        }
    }

    /// Reads a serialised entry.
    pub fn entry_from_wire(&self, wire: &WireEntry) -> Result<Entry, WireError> {
        let mut change = Change {
            handled: wire.handled,
            effects: wire.effects.clone(),
            guards: wire.guards.clone(),
            ..Change::default()
        };
        for op in &wire.ops {
            change.ops.push(self.op_from_wire(op)?);
        }
        for step in &wire.microsteps {
            let mut ids = Vec::with_capacity(step.len());
            for r in step {
                ids.push(self.trans_id(r).ok_or_else(|| {
                    WireError::UnknownTransition(alloc::format!("{}[{}]", r.source, r.index))
                })?);
            }
            change.microsteps.push(ids.len() as u32);
            change.microsteps.extend(ids);
        }
        Ok(Entry {
            seq: wire.seq,
            kind: match &wire.kind {
                WireEntryKind::Start { input } => EntryKind::Start {
                    input: input.clone(),
                },
                WireEntryKind::Event(event) => EntryKind::Event(event.clone()),
                WireEntryKind::Restore { to } => EntryKind::Restore { to: *to },
            },
            change,
        })
    }
}

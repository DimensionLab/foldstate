// SPDX-License-Identifier: Apache-2.0
//! The kernel: `(snapshot, event) -> (snapshot, effects)`. See `docs/semantics.md`; the
//! section names there are the function names here.
//!
//! Every change to the snapshot goes through a reversible [`Op`]. The list of ops is the
//! log entry's delta, and undoing it is how a failed step leaves the snapshot untouched.

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use serde_json::{Map, Value};

use crate::Guards;
use crate::NoGuards;
use crate::def::{Action, CmpOp, Effect, Event, Expr, Kind, Unhandled};
use crate::error::{EvalError, StepError};
use crate::machine::{CGuard, Domain, Machine, ROOT, StateId, Trans, TransId};
use crate::small::Small;
use crate::snapshot::{Op, Snapshot};
use crate::value::{self, Arith};
use crate::wire::{GuardResult, Invocation, Status, Timer, TransitionRef};

/// Name of the event that expressions see while a machine starts.
pub const INIT_EVENT: &str = "foldstate.init";

/// What one event did.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Change {
    /// False when the event enabled no transition and was ignored.
    pub handled: bool,
    /// What the host must do, in order.
    pub effects: Vec<Effect>,
    /// Results of host guards, in evaluation order.
    pub guards: Vec<GuardResult>,
    pub(crate) ops: Vec<Op>,
    /// The transitions taken, as `count, id, id, …` for each microstep in turn.
    pub(crate) microsteps: Vec<TransId>,
}

/// A new snapshot and what led to it.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub snapshot: Snapshot,
    pub change: Change,
}

enum Fail {
    Eval(EvalError),
    Livelock,
}

impl From<EvalError> for Fail {
    fn from(e: EvalError) -> Self {
        Fail::Eval(e)
    }
}

/// A sorted set of states.
type StateSet = Small;

struct Run<'a> {
    m: &'a Machine,
    snap: &'a mut Snapshot,
    guards: &'a dyn Guards,
    external: &'a Event,
    /// The internal event being processed, if any; otherwise the external one is current.
    internal: Option<Event>,
    now: Option<i64>,
    queue: VecDeque<Event>,
    to_invoke: StateSet,
    /// Eventless transitions that may have become enabled since they were last looked at.
    dirty: Vec<TransId>,
    out: Change,
    microsteps: u32,
}

impl<'a> Run<'a> {
    fn new(
        m: &'a Machine,
        snap: &'a mut Snapshot,
        guards: &'a dyn Guards,
        external: &'a Event,
    ) -> Self {
        Run {
            m,
            snap,
            guards,
            external,
            internal: None,
            now: external.at,
            queue: VecDeque::new(),
            to_invoke: StateSet::default(),
            dirty: Vec::new(),
            out: Change {
                handled: true,
                ..Change::default()
            },
            microsteps: 0,
        }
    }

    fn event(&self) -> &Event {
        self.internal.as_ref().unwrap_or(self.external)
    }

    fn push(&mut self, op: Op) -> Result<(), EvalError> {
        self.snap.apply(&op)?;
        self.out.ops.push(op);
        Ok(())
    }

    // ---- expressions, guards, actions ---------------------------------------------------

    fn eval(&self, e: &Expr) -> Result<Value, EvalError> {
        eval(&self.snap.context, self.event(), e)
    }

    /// `Err`: the guard cannot be evaluated, which makes the whole guard false.
    fn guard(&mut self, g: &CGuard) -> Result<bool, EvalError> {
        Ok(match g {
            CGuard::In(s) => self.snap.is_active(*s),
            CGuard::Not(g) => !self.guard(g)?,
            CGuard::All(gs) => {
                for g in gs {
                    if !self.guard(g)? {
                        return Ok(false);
                    }
                }
                true
            }
            CGuard::Any(gs) => {
                for g in gs {
                    if self.guard(g)? {
                        return Ok(true);
                    }
                }
                false
            }
            CGuard::Cmp { left, op, right } => compare(*op, &self.eval(left)?, &self.eval(right)?),
            CGuard::Named { name, params } => {
                let result = self
                    .guards
                    .eval(name, params, &self.snap.context, self.event());
                self.out.guards.push(GuardResult {
                    name: name.clone(),
                    params: params.clone(),
                    result,
                });
                result.ok_or_else(|| EvalError::UnknownGuard(name.clone()))?
            }
        })
    }

    fn holds(&mut self, t: &Trans) -> bool {
        match &t.guard {
            None => true,
            Some(g) => self.guard(g).unwrap_or(false),
        }
    }

    fn execute(&mut self, actions: &[Action]) -> Result<(), EvalError> {
        for a in actions {
            match a {
                Action::Assign { path, value } => {
                    let v = self.eval(value)?;
                    let plan = value::plan_set(&self.snap.context, path, v)?;
                    self.push(Op::Set {
                        path: plan.path,
                        before: plan.before,
                        after: Some(plan.after),
                    })?;
                }
                Action::Raise { event, data } => {
                    let data = match data {
                        Some(e) => self.eval(e)?,
                        None => Value::Null,
                    };
                    self.queue.push_back(Event {
                        name: event.clone(),
                        data,
                        at: self.now,
                    });
                }
                Action::Emit { name, params } => {
                    let params = match params {
                        Some(e) => self.eval(e)?,
                        None => Value::Null,
                    };
                    self.out.effects.push(Effect::Emit {
                        name: name.clone(),
                        params,
                    });
                }
                Action::Schedule {
                    id,
                    event,
                    delay_ms,
                } => self.schedule(id, event, *delay_ms)?,
                Action::Cancel { id } => self.cancel(id)?,
            }
        }
        Ok(())
    }

    fn schedule(&mut self, id: &str, event: &str, delay_ms: u64) -> Result<(), EvalError> {
        let due_ms = self
            .now
            .map(|n| n.saturating_add(i64::try_from(delay_ms).unwrap_or(i64::MAX)));
        let timer = Timer {
            event: event.to_string(),
            delay_ms,
            due_ms,
        };
        self.push(Op::Timer {
            id: id.to_string(),
            before: self.snap.timers.get(id).cloned(),
            after: Some(timer),
        })?;
        self.out.effects.push(Effect::Schedule {
            id: id.to_string(),
            event: event.to_string(),
            delay_ms,
            due_ms,
        });
        Ok(())
    }

    fn cancel(&mut self, id: &str) -> Result<(), EvalError> {
        if let Some(before) = self.snap.timers.get(id).cloned() {
            self.push(Op::Timer {
                id: id.to_string(),
                before: Some(before),
                after: None,
            })?;
            self.out.effects.push(Effect::Cancel { id: id.to_string() });
        }
        Ok(())
    }

    // ---- structure ------------------------------------------------------------------------

    fn history_default(&self, h: StateId) -> Vec<StateId> {
        history_default(self.m, h)
    }

    fn effective_targets(&self, t: &Trans) -> Vec<StateId> {
        effective_targets(self.m, self.snap, t)
    }

    fn domain(&self, t: &Trans) -> Option<StateId> {
        domain(self.m, self.snap, t)
    }

    fn in_final_state(&self, s: StateId) -> bool {
        let st = self.m.state(s);
        match st.kind {
            Kind::Compound => st
                .children
                .iter()
                .any(|&c| self.m.state(c).kind == Kind::Final && self.snap.is_active(c)),
            Kind::Parallel => st.children.iter().all(|&c| self.in_final_state(c)),
            _ => false,
        }
    }

    // ---- selecting transitions ------------------------------------------------------------

    /// Transitions whose descriptor matches, whose source is active and whose guard holds,
    /// in document order.
    fn enabled_for_event(&mut self, name: &str) -> Small {
        let m = self.m;
        let exact: &[TransId] = m.exact.get(name).map(Vec::as_slice).unwrap_or(&[]);
        let mut candidates = Small::default();
        candidates.extend(exact);
        if !m.patterns.is_empty() {
            for &t in &m.patterns {
                if m.tr(t)
                    .desc
                    .as_ref()
                    .is_some_and(|ds| ds.iter().any(|d| d.matches(name)))
                {
                    candidates.push(t);
                }
            }
            candidates.sort_dedup();
        }
        candidates.retain(|t| {
            let tr = m.tr(t);
            self.snap.is_active(tr.source) && self.holds(tr)
        });
        candidates
    }

    /// The enabled eventless transitions, in document order.
    ///
    /// Between macrosteps none is enabled. One can become enabled only when its source is
    /// entered, when a state its guard asks about is entered or exited, when its guard
    /// reads something else (context, event, host), or when it was enabled the last time.
    /// Only those are looked at.
    fn enabled_eventless(&mut self) -> Small {
        let m = self.m;
        let mut candidates = Small::default();
        if !m.has_eventless {
            return candidates;
        }
        candidates.extend(&self.dirty);
        self.dirty.clear();
        candidates.extend(&m.volatile);
        candidates.sort_dedup();
        candidates.retain(|t| {
            let tr = m.tr(t);
            self.snap.is_active(tr.source) && self.holds(tr)
        });
        candidates
    }

    /// Notes that a state was entered or exited.
    fn touched(&mut self, s: StateId, entered: bool) {
        let m = self.m;
        if !m.has_eventless {
            return;
        }
        if entered {
            self.dirty.extend_from_slice(&m.state(s).eventless);
        }
        self.dirty.extend_from_slice(&m.watchers[s as usize]);
    }

    /// From the enabled transitions (in document order) to the ones taken: per active atomic
    /// state the first enabled transition of the nearest state that has one, then without
    /// the conflicting ones.
    fn choose(&self, enabled: Small) -> Small {
        if enabled.len() <= 1 {
            return enabled;
        }
        let mut taken = Small::default();
        taken.extend(&choose(self.m, self.snap, enabled.as_slice().to_vec()));
        taken
    }

    // ---- taking transitions ---------------------------------------------------------------

    fn microstep(&mut self, taken: &[TransId]) -> Result<(), Fail> {
        let m = self.m;
        self.microsteps += 1;
        if self.microsteps > m.def.max_microsteps {
            return Err(Fail::Livelock);
        }
        if self.out.ops.capacity() == 0 {
            self.out.ops.reserve(8);
        }
        self.out.microsteps.push(taken.len() as u32);
        self.out.microsteps.extend_from_slice(taken);

        // Exit.
        let mut exit = Small::default();
        for &t in taken {
            if let Some(d) = self.domain(m.tr(t)) {
                exit.extend(self.snap.active_in(d + 1, m.state(d).last));
            }
        }
        if taken.len() > 1 {
            exit.sort_dedup();
        }
        let exit = exit.as_slice();
        for &s in exit {
            self.to_invoke.remove_sorted(s);
        }
        for &s in exit {
            let st = m.state(s);
            for &h in &st.history {
                let deep = m.state(h).deep;
                let value: Vec<StateId> = self
                    .snap
                    .active_in(s + 1, st.last)
                    .iter()
                    .copied()
                    .filter(|&x| {
                        if deep {
                            m.is_atomic(x)
                        } else {
                            m.state(x).parent == Some(s)
                        }
                    })
                    .collect();
                let before = self.snap.history.get(&h).cloned();
                if before.as_ref() != Some(&value) {
                    self.push(Op::History {
                        state: h,
                        before,
                        after: Some(value),
                    })?;
                }
            }
        }
        for &s in exit.iter().rev() {
            let st = m.state(s);
            self.execute(&st.exit)?;
            for a in &st.after {
                self.cancel(&a.id)?;
            }
            for v in &st.invoke {
                if let Some(before) = self.snap.invocations.get(&v.id).cloned() {
                    self.push(Op::Invocation {
                        id: v.id.clone(),
                        before: Some(before),
                        after: None,
                    })?;
                    self.out.effects.push(Effect::Stop { id: v.id.clone() });
                }
            }
            self.push(Op::Exit(s))?;
            self.touched(s, false);
        }

        // Transition actions.
        for &t in taken {
            self.execute(&m.tr(t).actions)?;
        }

        // Enter. The entry set is computed after the exits: it depends on the history they
        // recorded.
        let mut set = StateSet::default();
        for &t in taken {
            let tr = m.tr(t);
            for &s in &tr.targets {
                self.add_descendants(s, &mut set);
            }
            let ancestor = self.domain(tr);
            if tr.domain == Domain::Dynamic {
                for s in self.effective_targets(tr) {
                    self.add_ancestors(s, ancestor, &mut set);
                }
            } else {
                // Without history targets the effective targets are the targets.
                for &s in &tr.targets {
                    self.add_ancestors(s, ancestor, &mut set);
                }
            }
        }
        self.enter(set.as_slice())?;
        Ok(())
    }

    fn enter(&mut self, states: &[StateId]) -> Result<(), EvalError> {
        let m = self.m;
        for &s in states {
            if self.snap.is_active(s) {
                // In the entry set without having been exited (a history state of an
                // ancestor, targeted from inside it): it stays as it is.
                continue;
            }
            let st = m.state(s);
            self.push(Op::Enter(s))?;
            self.touched(s, true);
            if !st.invoke.is_empty() {
                self.to_invoke.insert_sorted(s);
            }
            self.execute(&st.entry)?;
            for a in &st.after {
                self.schedule(&a.id, &a.event, a.delay_ms)?;
            }
            if st.kind == Kind::Final {
                let Some(parent) = st.parent else { continue };
                if parent == ROOT {
                    self.finish()?;
                    continue;
                }
                self.raise_done(parent);
                let Some(grandparent) = m.state(parent).parent else {
                    continue;
                };
                let gp = m.state(grandparent);
                if gp.kind == Kind::Parallel && gp.children.iter().all(|&c| self.in_final_state(c))
                {
                    if grandparent == ROOT {
                        self.finish()?;
                    } else {
                        self.raise_done(grandparent);
                    }
                }
            }
        }
        Ok(())
    }

    fn raise_done(&mut self, s: StateId) {
        self.queue.push_back(Event {
            name: format!("done.state.{}", self.m.state(s).path),
            data: Value::Null,
            at: self.now,
        });
    }

    fn finish(&mut self) -> Result<(), EvalError> {
        if self.snap.status == Status::Active {
            self.push(Op::Status {
                before: Status::Active,
                after: Status::Done,
            })?;
        }
        Ok(())
    }

    fn add_descendants(&self, state: StateId, set: &mut StateSet) {
        let m = self.m;
        let st = m.state(state);
        match st.kind {
            Kind::History => {
                let targets = match self.snap.history.get(&state) {
                    Some(v) => v.clone(),
                    None => self.history_default(state),
                };
                for &s in &targets {
                    self.add_descendants(s, set);
                }
                for &s in &targets {
                    self.add_ancestors(s, st.parent, set);
                }
            }
            Kind::Compound => {
                set.insert_sorted(state);
                if let Some(s) = st.initial {
                    self.add_descendants(s, set);
                    self.add_ancestors(s, Some(state), set);
                }
            }
            Kind::Parallel => {
                set.insert_sorted(state);
                self.complete_regions(state, set);
            }
            Kind::Atomic | Kind::Final => set.insert_sorted(state),
        }
    }

    fn add_ancestors(&self, state: StateId, upto: Option<StateId>, set: &mut StateSet) {
        let m = self.m;
        let mut ancestors = Small::default();
        let mut p = m.state(state).parent;
        while let Some(a) = p {
            if Some(a) == upto {
                break;
            }
            set.insert_sorted(a);
            ancestors.push(a);
            p = m.state(a).parent;
        }
        // The root is never entered, but when a transition's domain is a parallel root, the
        // regions it emptied are entered again.
        if upto == Some(ROOT) && m.state(ROOT).kind == Kind::Parallel {
            ancestors.push(ROOT);
        }
        for &a in ancestors.as_slice() {
            if m.state(a).kind == Kind::Parallel {
                self.complete_regions(a, set);
            }
        }
    }

    /// Enters by default every region of `parallel` that nothing in the set is inside of
    /// and that is not still active (a region that was not exited is complete as it is).
    fn complete_regions(&self, parallel: StateId, set: &mut StateSet) {
        let m = self.m;
        for &child in &m.state(parallel).children {
            if !set.any_in(child + 1, m.state(child).last) && !self.snap.is_active(child) {
                self.add_descendants(child, set);
            }
        }
    }

    // ---- macrostep ------------------------------------------------------------------------

    /// The rest of a macrostep after its first microstep.
    fn settle(&mut self) -> Result<(), Fail> {
        let m = self.m;
        while self.snap.status == Status::Active {
            let eventless = self.enabled_eventless();
            // Whatever is enabled now stays a candidate: taken or not, it is enabled again
            // next time unless its source is exited or its guard changes.
            self.dirty.extend_from_slice(eventless.as_slice());
            let mut taken = self.choose(eventless);
            if taken.is_empty() {
                let Some(internal) = self.queue.pop_front() else {
                    break;
                };
                let name = internal.name.clone();
                self.internal = Some(internal);
                let enabled = self.enabled_for_event(&name);
                taken = self.choose(enabled);
            }
            if !taken.is_empty() {
                self.microstep(taken.as_slice())?;
            }
        }
        if self.snap.status == Status::Active {
            let pending = core::mem::take(&mut self.to_invoke);
            for &s in pending.as_slice() {
                if !self.snap.is_active(s) {
                    continue;
                }
                let st = m.state(s);
                for v in &st.invoke {
                    let params = match &v.params {
                        Some(e) => self.eval(e)?,
                        None => Value::Null,
                    };
                    self.push(Op::Invocation {
                        id: v.id.clone(),
                        before: self.snap.invocations.get(&v.id).cloned(),
                        after: Some(Invocation {
                            state: st.path.clone(),
                            name: v.name.clone(),
                            params: params.clone(),
                        }),
                    })?;
                    self.out.effects.push(Effect::Start {
                        id: v.id.clone(),
                        name: v.name.clone(),
                        params,
                    });
                }
            }
        } else {
            // Finished: nothing is left running or pending.
            let invocations: Vec<(String, Invocation)> = self
                .snap
                .invocations
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            for (id, before) in invocations {
                self.push(Op::Invocation {
                    id: id.clone(),
                    before: Some(before),
                    after: None,
                })?;
                self.out.effects.push(Effect::Stop { id });
            }
            let timers: Vec<String> = self.snap.timers.keys().cloned().collect();
            for id in timers {
                self.cancel(&id)?;
            }
        }
        Ok(())
    }
}

pub(crate) fn eval(context: &Value, event: &Event, e: &Expr) -> Result<Value, EvalError> {
    Ok(match e {
        Expr::Lit(v) => v.clone(),
        Expr::Ctx(p) => context.pointer(p).cloned().unwrap_or(Value::Null),
        Expr::Event(p) => event.data.pointer(p).cloned().unwrap_or(Value::Null),
        Expr::Add(a, b) => value::arith(
            Arith::Add,
            &eval(context, event, a)?,
            &eval(context, event, b)?,
        )?,
        Expr::Sub(a, b) => value::arith(
            Arith::Sub,
            &eval(context, event, a)?,
            &eval(context, event, b)?,
        )?,
        Expr::Mul(a, b) => value::arith(
            Arith::Mul,
            &eval(context, event, a)?,
            &eval(context, event, b)?,
        )?,
    })
}

pub(crate) fn compare(op: CmpOp, l: &Value, r: &Value) -> bool {
    match op {
        CmpOp::Eq => value::equal(l, r),
        CmpOp::Ne => !value::equal(l, r),
        CmpOp::Lt => value::compare(l, r).is_some_and(|o| o.is_lt()),
        CmpOp::Le => value::compare(l, r).is_some_and(|o| o.is_le()),
        CmpOp::Gt => value::compare(l, r).is_some_and(|o| o.is_gt()),
        CmpOp::Ge => value::compare(l, r).is_some_and(|o| o.is_ge()),
    }
}

pub(crate) fn history_default(m: &Machine, h: StateId) -> Vec<StateId> {
    let st = m.state(h);
    if !st.default.is_empty() {
        return st.default.clone();
    }
    let Some(parent) = st.parent else {
        return Vec::new();
    };
    let p = m.state(parent);
    match (p.kind, p.initial) {
        (Kind::Compound, Some(i)) => alloc::vec![i],
        _ => p.children.clone(),
    }
}

/// The targets of a transition with every history state replaced by what it restores.
pub(crate) fn effective_targets(m: &Machine, snap: &Snapshot, t: &Trans) -> Vec<StateId> {
    fn add(m: &Machine, snap: &Snapshot, s: StateId, out: &mut Vec<StateId>) {
        if m.state(s).kind == Kind::History {
            match snap.history.get(&s) {
                Some(v) => {
                    for &x in v {
                        if !out.contains(&x) {
                            out.push(x);
                        }
                    }
                }
                None => {
                    for x in history_default(m, s) {
                        add(m, snap, x, out);
                    }
                }
            }
        } else if !out.contains(&s) {
            out.push(s);
        }
    }
    let mut out = Vec::new();
    for &s in &t.targets {
        add(m, snap, s, &mut out);
    }
    out
}

pub(crate) fn domain(m: &Machine, snap: &Snapshot, t: &Trans) -> Option<StateId> {
    match t.domain {
        Domain::None => None,
        Domain::Fixed(d) => Some(d),
        Domain::Dynamic => {
            let effective = effective_targets(m, snap, t);
            if effective.is_empty() {
                None
            } else {
                Some(m.domain_for(t, &effective))
            }
        }
    }
}

/// The range of state numbers a transition exits, if it exits anything.
fn exit_range(m: &Machine, snap: &Snapshot, t: TransId) -> Option<(StateId, StateId)> {
    let d = domain(m, snap, m.tr(t))?;
    let last = m.state(d).last;
    (d < last).then_some((d + 1, last))
}

/// Two transitions conflict when the sets of states they exit intersect.
fn conflict(m: &Machine, snap: &Snapshot, a: TransId, b: TransId) -> bool {
    let (Some((lo1, hi1)), Some((lo2, hi2))) = (exit_range(m, snap, a), exit_range(m, snap, b))
    else {
        return false;
    };
    !snap.active_in(lo1.max(lo2), hi1.min(hi2)).is_empty()
}

pub(crate) fn choose(m: &Machine, snap: &Snapshot, enabled: Vec<TransId>) -> Vec<TransId> {
    if enabled.len() <= 1 {
        return enabled;
    }
    let chosen = choose_per_atomic(m, snap, &enabled);
    remove_conflicts(m, snap, chosen)
}

/// Per active atomic state, in document order: the first enabled transition of the nearest
/// state at or above it that has one. `enabled` is in document order.
pub(crate) fn choose_per_atomic(m: &Machine, snap: &Snapshot, enabled: &[TransId]) -> Vec<TransId> {
    let mut atomic: Vec<StateId> = Vec::new();
    for &t in enabled {
        let s = m.tr(t).source;
        atomic.extend(
            snap.active_in(s, m.state(s).last)
                .iter()
                .copied()
                .filter(|&x| m.is_atomic(x)),
        );
    }
    atomic.sort_unstable();
    atomic.dedup();
    let mut chosen: Vec<TransId> = Vec::new();
    for a in atomic {
        let mut best: Option<(u32, TransId)> = None;
        for &t in enabled {
            let s = m.tr(t).source;
            if s == a || m.is_descendant(a, s) {
                let depth = m.state(s).depth;
                if best.is_none_or(|(d, _)| depth > d) {
                    best = Some((depth, t));
                }
            }
        }
        if let Some((_, t)) = best
            && !chosen.contains(&t)
        {
            chosen.push(t);
        }
    }
    chosen
}

/// Drops, in order, every transition that conflicts with one already kept, unless its
/// source is a descendant of that one's source, in which case it replaces it.
pub(crate) fn remove_conflicts(m: &Machine, snap: &Snapshot, chosen: Vec<TransId>) -> Vec<TransId> {
    if chosen.len() <= 1 {
        return chosen;
    }
    let mut kept: Vec<TransId> = Vec::new();
    for t1 in chosen {
        let mut preempted = false;
        let mut replaced: Vec<TransId> = Vec::new();
        for &t2 in &kept {
            if conflict(m, snap, t1, t2) {
                if m.is_descendant(m.tr(t1).source, m.tr(t2).source) {
                    replaced.push(t2);
                } else {
                    preempted = true;
                    break;
                }
            }
        }
        if !preempted {
            kept.retain(|t| !replaced.contains(t));
            kept.push(t1);
        }
    }
    kept
}

impl Change {
    /// The transitions taken, one list per microstep. The first list of a start is empty.
    pub fn microsteps(&self, machine: &Machine) -> Vec<Vec<TransitionRef>> {
        let mut out = Vec::new();
        let mut rest = self.microsteps.as_slice();
        while let Some((&n, tail)) = rest.split_first() {
            let (ids, tail) = tail.split_at((n as usize).min(tail.len()));
            out.push(ids.iter().map(|&t| machine.trans_ref(t)).collect());
            rest = tail;
        }
        out
    }

    /// Paths of the states that are active after the event and were not before.
    pub fn entered<'m>(&self, machine: &'m Machine) -> Vec<&'m str> {
        self.net(machine, true)
    }

    /// Paths of every state that became active at some point while the event was processed,
    /// in document order, including those left again before the end.
    pub fn visited<'m>(&self, machine: &'m Machine) -> Vec<&'m str> {
        let mut ids: Vec<StateId> = self
            .ops
            .iter()
            .filter_map(|op| match op {
                Op::Enter(s) if *s != ROOT => Some(*s),
                _ => None,
            })
            .collect();
        ids.sort_unstable();
        ids.dedup();
        ids.into_iter()
            .map(|s| machine.state(s).path.as_str())
            .collect()
    }

    /// Paths of the states that were active before the event and are not after.
    pub fn exited<'m>(&self, machine: &'m Machine) -> Vec<&'m str> {
        self.net(machine, false)
    }

    fn net<'m>(&self, machine: &'m Machine, entered: bool) -> Vec<&'m str> {
        // A state can be exited and entered again within one macrostep; only the net
        // difference counts.
        let mut balance: alloc::collections::BTreeMap<StateId, i32> = Default::default();
        for op in &self.ops {
            match op {
                Op::Enter(s) => *balance.entry(*s).or_default() += 1,
                Op::Exit(s) => *balance.entry(*s).or_default() -= 1,
                _ => {}
            }
        }
        balance
            .into_iter()
            .filter(|&(s, n)| s != ROOT && if entered { n > 0 } else { n < 0 })
            .map(|(s, _)| machine.state(s).path.as_str())
            .collect()
    }

    /// Applies this change to the snapshot it was computed from.
    pub fn redo(&self, snapshot: &mut Snapshot) -> Result<(), EvalError> {
        snapshot.apply_all(&self.ops)
    }

    /// Takes this change back out of the snapshot it produced.
    pub fn undo(&self, snapshot: &mut Snapshot) -> Result<(), EvalError> {
        snapshot.revert_all(&self.ops)
    }

    /// True when the event changed nothing at all.
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty() && self.effects.is_empty()
    }
}

impl Machine {
    /// Starts the machine. `input` is null or an object whose keys are written over the
    /// definition's context.
    pub fn start(&self, input: Value) -> Result<Step, StepError> {
        self.start_with(input, &NoGuards)
    }

    /// [`Machine::start`] with host guards.
    pub fn start_with(&self, input: Value, guards: &dyn Guards) -> Result<Step, StepError> {
        let mut context = match &self.def.context {
            Value::Null => Value::Object(Map::new()),
            other => other.clone(),
        };
        if let (Value::Object(c), Value::Object(i)) = (&mut context, &input) {
            for (k, v) in i {
                c.insert(k.clone(), v.clone());
            }
        }
        let init = Event {
            name: INIT_EVENT.into(),
            data: input,
            at: None,
        };
        let mut snapshot = Snapshot::blank(self.hash);
        let mut run = Run::new(self, &mut snapshot, guards, &init);
        let result = (|| -> Result<(), Fail> {
            run.push(Op::Set {
                path: String::new(),
                before: Some(Value::Null),
                after: Some(context),
            })?;
            run.microsteps = 1;
            run.out.microsteps.push(0);
            let mut set = StateSet::default();
            run.add_descendants(ROOT, &mut set);
            run.enter(set.as_slice())?;
            run.settle()
        })();
        let change = run.out;
        match result {
            Ok(()) => Ok(Step { snapshot, change }),
            Err(f) => Err(self.fail(INIT_EVENT, f)),
        }
    }

    fn fail(&self, event: &str, f: Fail) -> StepError {
        match f {
            Fail::Eval(error) => StepError::Eval {
                event: event.to_string(),
                error,
            },
            Fail::Livelock => StepError::Livelock {
                event: event.to_string(),
                limit: self.def.max_microsteps,
            },
        }
    }

    /// Processes one event and returns the new snapshot. `snapshot` is not changed.
    pub fn step(&self, snapshot: &Snapshot, event: &Event) -> Result<Step, StepError> {
        self.step_with(snapshot, event, &NoGuards)
    }

    /// [`Machine::step`] with host guards.
    pub fn step_with(
        &self,
        snapshot: &Snapshot,
        event: &Event,
        guards: &dyn Guards,
    ) -> Result<Step, StepError> {
        let mut snapshot = snapshot.clone();
        let change = self.step_in_place(&mut snapshot, event, guards)?;
        Ok(Step { snapshot, change })
    }

    /// Processes one event, updating `snapshot`. On error the snapshot is left as it was.
    pub fn step_in_place(
        &self,
        snapshot: &mut Snapshot,
        event: &Event,
        guards: &dyn Guards,
    ) -> Result<Change, StepError> {
        if snapshot.machine != self.hash {
            return Err(StepError::MachineMismatch);
        }
        if snapshot.status == Status::Done {
            return Err(StepError::Done {
                event: event.name.clone(),
            });
        }
        let mut run = Run::new(self, snapshot, guards, event);
        let result = (|| -> Result<bool, Fail> {
            // A delivered timer is no longer pending.
            let delivered: Vec<(String, Timer)> = if run.snap.timers.is_empty() {
                Vec::new()
            } else {
                run.snap
                    .timers
                    .iter()
                    .filter(|(_, t)| t.event == event.name)
                    .map(|(k, t)| (k.clone(), t.clone()))
                    .collect()
            };
            // Events the kernel produces itself cannot be sent from outside: a `done.state`
            // event never, a delayed transition's event only while its timer is pending.
            if event.name.starts_with("done.state.")
                || (delivered.is_empty()
                    && event.name.starts_with("after.")
                    && self.after_events.contains(&event.name))
            {
                return Ok(false);
            }
            for (id, before) in delivered {
                run.push(Op::Timer {
                    id,
                    before: Some(before),
                    after: None,
                })?;
            }
            let enabled = run.enabled_for_event(&event.name);
            let taken = run.choose(enabled);
            if taken.is_empty() {
                return Ok(false);
            }
            run.microstep(taken.as_slice())?;
            run.settle()?;
            Ok(true)
        })();
        let mut change = run.out;
        let reject = |snapshot: &mut Snapshot, change: &Change| {
            // Ops were valid when applied, so undoing them cannot fail.
            let _ = snapshot.revert_all(&change.ops);
        };
        match result {
            Ok(true) => Ok(change),
            Ok(false) => {
                let ignored = self.def.unhandled == Unhandled::Ignore
                    || self.ignore.iter().any(|d| d.matches(&event.name));
                if ignored {
                    change.handled = false;
                    Ok(change)
                } else {
                    reject(snapshot, &change);
                    Err(StepError::Rejected {
                        event: event.name.clone(),
                    })
                }
            }
            Err(f) => {
                reject(snapshot, &change);
                Err(self.fail(&event.name, f))
            }
        }
    }
}

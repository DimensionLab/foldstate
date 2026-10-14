// SPDX-License-Identifier: Apache-2.0
//! Looking at a machine without running it: the states and transitions it has, which events
//! it would accept now, and why an event does or does not cause a transition.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use serde::Serialize;
use serde_json::Value;

use crate::Guards;
use crate::def::{Action, Event, Expr, Guard, Kind, Unhandled};
use crate::kernel::{choose_per_atomic, compare, eval, remove_conflicts};
use crate::machine::{CGuard, Machine, StateId, TransId};
use crate::snapshot::Snapshot;
use crate::wire::{Status, TransitionRef};

/// A state of a machine.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StateInfo<'m> {
    /// Empty for the root.
    pub path: &'m str,
    pub kind: Kind,
    pub parent: Option<&'m str>,
    /// Child states, then history states, in document order within each group.
    pub children: Vec<&'m str>,
    /// Compound states: the child entered by default.
    pub initial: Option<&'m str>,
    pub tags: &'m [String],
    pub meta: &'m Value,
}

/// A transition of a machine.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TransitionInfo<'m> {
    pub reference: TransitionRef,
    /// The event descriptors as written; `None` for an eventless transition.
    pub event: Option<&'m str>,
    pub targets: Vec<&'m str>,
    pub internal: bool,
    pub guard: Option<&'m Guard>,
    pub actions: &'m [Action],
    /// Set for transitions that come from `after`.
    pub delay_ms: Option<u64>,
}

/// What would happen to an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// At least one transition would be taken.
    Handled,
    /// No transition; the definition ignores the event.
    Ignored,
    /// No transition; the step would fail.
    Rejected,
    /// The machine has finished.
    Done,
}

/// Why a transition is or is not taken for an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Taken,
    /// The source state is not active.
    SourceInactive,
    /// The guard is false or cannot be evaluated.
    GuardFalse,
    /// Enabled, but for every active state below its source a nearer state or an earlier
    /// transition handles the event.
    Shadowed,
    /// Enabled and chosen, then dropped because it conflicts with a transition that takes
    /// priority.
    Preempted,
    /// The event is one the kernel produces itself (`done.state.*`, or the event of a timer
    /// that is not pending) and is not accepted from outside.
    Reserved,
}

/// The evaluation of a guard, part by part.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GuardTrace {
    pub guard: Guard,
    /// `None`: could not be evaluated, which makes the whole guard false.
    pub result: Option<bool>,
    /// The parts that were evaluated. Evaluation stops at the part that decides the result.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<GuardTrace>,
    /// For comparisons: the two values compared.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub values: Option<(Value, Value)>,
}

/// One transition that the event could trigger.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Candidate {
    pub transition: TransitionRef,
    pub targets: Vec<String>,
    pub verdict: Verdict,
    pub guard: Option<GuardTrace>,
}

/// Why an event does what it does in a given snapshot. Covers the event's own transitions
/// (the first microstep); what follows from them is in the log entry.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Explanation {
    pub outcome: Outcome,
    /// Every transition whose event descriptor matches, in document order.
    pub candidates: Vec<Candidate>,
}

/// An event descriptor that would cause a transition now.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EnabledEvent {
    /// The descriptor as written in the definition (`step.done`, `step.*`, `*`).
    pub event: String,
    pub transitions: Vec<TransitionRef>,
    /// True when a guard reads the event's data or asks the host, so the answer can depend
    /// on the event actually sent. Such guards are assumed to hold.
    pub depends_on_event: bool,
}

struct Tracer<'a> {
    m: &'a Machine,
    snap: &'a Snapshot,
    event: &'a Event,
    guards: &'a dyn Guards,
}

impl Tracer<'_> {
    fn expr(&self, e: &Expr) -> Option<Value> {
        eval(&self.snap.context, self.event, e).ok()
    }

    fn trace(&self, written: &Guard, compiled: &CGuard) -> GuardTrace {
        let mut t = GuardTrace {
            guard: written.clone(),
            result: None,
            parts: Vec::new(),
            values: None,
        };
        match (written, compiled) {
            (Guard::In(_), CGuard::In(s)) => t.result = Some(self.snap.is_active(*s)),
            (Guard::Not(w), CGuard::Not(c)) => {
                let part = self.trace(w, c);
                t.result = part.result.map(|r| !r);
                t.parts.push(part);
            }
            (Guard::All(ws), CGuard::All(cs)) => {
                t.result = Some(true);
                for (w, c) in ws.iter().zip(cs) {
                    let part = self.trace(w, c);
                    let r = part.result;
                    t.parts.push(part);
                    if r != Some(true) {
                        t.result = r;
                        break;
                    }
                }
            }
            (Guard::Any(ws), CGuard::Any(cs)) => {
                t.result = Some(false);
                for (w, c) in ws.iter().zip(cs) {
                    let part = self.trace(w, c);
                    let r = part.result;
                    t.parts.push(part);
                    if r != Some(false) {
                        t.result = r;
                        break;
                    }
                }
            }
            (Guard::Cmp { left, op, right }, _) => {
                if let (Some(l), Some(r)) = (self.expr(left), self.expr(right)) {
                    t.result = Some(compare(*op, &l, &r));
                    t.values = Some((l, r));
                }
            }
            (Guard::Named { name, params }, _) => {
                t.result = self
                    .guards
                    .eval(name, params, &self.snap.context, self.event);
            }
            _ => {}
        }
        t
    }
}

fn reads_event(g: &CGuard) -> bool {
    fn expr(e: &Expr) -> bool {
        match e {
            Expr::Event(_) => true,
            Expr::Lit(_) | Expr::Ctx(_) => false,
            Expr::Add(a, b) | Expr::Sub(a, b) | Expr::Mul(a, b) => expr(a) || expr(b),
        }
    }
    match g {
        CGuard::In(_) => false,
        CGuard::Not(g) => reads_event(g),
        CGuard::All(gs) | CGuard::Any(gs) => gs.iter().any(reads_event),
        CGuard::Cmp { left, right, .. } => expr(left) || expr(right),
        CGuard::Named { .. } => true,
    }
}

impl Machine {
    fn path_refs(&self, states: &[StateId]) -> Vec<&str> {
        states
            .iter()
            .map(|&s| self.state(s).path.as_str())
            .collect()
    }

    /// Every state, in document order, the root first.
    pub fn states(&self) -> impl Iterator<Item = StateInfo<'_>> {
        self.states.iter().map(|st| {
            let mut children = self.path_refs(&st.children);
            children.extend(self.path_refs(&st.history));
            StateInfo {
                path: &st.path,
                kind: st.kind,
                parent: st.parent.map(|p| self.state(p).path.as_str()),
                children,
                initial: st.initial.map(|i| self.state(i).path.as_str()),
                tags: &st.tags,
                meta: &st.meta,
            }
        })
    }

    fn transition_info(&self, t: TransId) -> TransitionInfo<'_> {
        let tr = self.tr(t);
        TransitionInfo {
            reference: self.trans_ref(t),
            event: tr.event_text.as_deref(),
            targets: self.path_refs(&tr.targets),
            internal: tr.internal,
            guard: tr.def_guard.as_ref(),
            actions: &tr.actions,
            delay_ms: tr.delay_ms,
        }
    }

    /// Every transition, in document order.
    pub fn transitions(&self) -> impl Iterator<Item = TransitionInfo<'_>> {
        (0..self.trans.len() as TransId).map(|t| self.transition_info(t))
    }

    /// The transition a log entry or an explanation refers to.
    pub fn transition(&self, reference: &TransitionRef) -> Option<TransitionInfo<'_>> {
        self.trans_id(reference).map(|t| self.transition_info(t))
    }

    /// The event names that transitions name exactly, sorted. Events matched only by a
    /// pattern (`a.*`, `*`) are not listed: there is no finite list of them.
    ///
    /// Events the kernel produces itself (`done.state.*` and the events of `after` timers)
    /// are left out: they cannot be sent from outside.
    pub fn event_names(&self) -> Vec<&str> {
        self.exact
            .keys()
            .filter(|k| !k.starts_with("done.state.") && !self.after_events.contains(*k))
            .map(String::as_str)
            .collect()
    }

    /// Evaluates a guard written against the root (use `#path` references) in a snapshot.
    /// `None`: the guard refers to a state that does not exist or cannot be evaluated.
    pub fn check_guard(
        &self,
        snapshot: &Snapshot,
        guard: &Guard,
        guards: &dyn Guards,
    ) -> Option<bool> {
        let compiled = self.compile_guard(guard)?;
        let probe = Event::default();
        Tracer {
            m: self,
            snap: snapshot,
            event: &probe,
            guards,
        }
        .trace(guard, &compiled)
        .result
    }

    fn compile_guard(&self, g: &Guard) -> Option<CGuard> {
        Some(match g {
            Guard::In(r) => CGuard::In(self.state_id(r.strip_prefix('#').unwrap_or(r))?),
            Guard::Not(g) => CGuard::Not(alloc::boxed::Box::new(self.compile_guard(g)?)),
            Guard::All(gs) => CGuard::All(
                gs.iter()
                    .map(|g| self.compile_guard(g))
                    .collect::<Option<_>>()?,
            ),
            Guard::Any(gs) => CGuard::Any(
                gs.iter()
                    .map(|g| self.compile_guard(g))
                    .collect::<Option<_>>()?,
            ),
            Guard::Cmp { left, op, right } => CGuard::Cmp {
                left: left.clone(),
                op: *op,
                right: right.clone(),
            },
            Guard::Named { name, params } => CGuard::Named {
                name: name.clone(),
                params: params.clone(),
            },
        })
    }

    /// Why `event` would or would not cause a transition in `snapshot`.
    pub fn explain(&self, snapshot: &Snapshot, event: &Event, guards: &dyn Guards) -> Explanation {
        let tracer = Tracer {
            m: self,
            snap: snapshot,
            event,
            guards,
        };
        let mut candidates: Vec<(TransId, Candidate)> = Vec::new();
        let mut enabled: Vec<TransId> = Vec::new();
        for (t, tr) in self.trans.iter().enumerate() {
            let t = t as TransId;
            let Some(desc) = &tr.desc else { continue };
            if !desc.iter().any(|d| d.matches(&event.name)) {
                continue;
            }
            let guard = match (&tr.def_guard, &tr.guard) {
                (Some(w), Some(c)) => Some(tracer.trace(w, c)),
                _ => None,
            };
            let verdict = if !snapshot.is_active(tr.source) {
                Verdict::SourceInactive
            } else if guard.as_ref().is_some_and(|g| g.result != Some(true)) {
                Verdict::GuardFalse
            } else {
                enabled.push(t);
                Verdict::Shadowed
            };
            candidates.push((
                t,
                Candidate {
                    transition: self.trans_ref(t),
                    targets: self
                        .path_refs(&tr.targets)
                        .into_iter()
                        .map(String::from)
                        .collect(),
                    verdict,
                    guard,
                },
            ));
        }
        // Events the kernel produces itself are not accepted from outside.
        if event.name.starts_with("done.state.")
            || (self.after_events.contains(&event.name)
                && !snapshot.timers.values().any(|t| t.event == event.name))
        {
            enabled.clear();
            for (_, c) in &mut candidates {
                if c.verdict == Verdict::Shadowed {
                    c.verdict = Verdict::Reserved;
                }
            }
        }
        let chosen = choose_per_atomic(tracer.m, snapshot, &enabled);
        let taken = remove_conflicts(self, snapshot, chosen.clone());
        for (t, c) in &mut candidates {
            if taken.contains(t) {
                c.verdict = Verdict::Taken;
            } else if chosen.contains(t) {
                c.verdict = Verdict::Preempted;
            }
        }
        let outcome = if snapshot.status == Status::Done {
            Outcome::Done
        } else if !taken.is_empty() {
            Outcome::Handled
        } else if self.def.unhandled == Unhandled::Ignore
            || self.ignore.iter().any(|d| d.matches(&event.name))
        {
            Outcome::Ignored
        } else {
            Outcome::Rejected
        };
        Explanation {
            outcome,
            candidates: candidates.into_iter().map(|(_, c)| c).collect(),
        }
    }

    /// The event descriptors that have a transition with an active source and a guard that
    /// holds, in document order of their first transition. Empty when the machine is done.
    ///
    /// This looks at each transition on its own. Whether a particular event is handled also
    /// depends on priority between transitions; [`Machine::explain`] answers that exactly.
    pub fn enabled(&self, snapshot: &Snapshot, guards: &dyn Guards) -> Vec<EnabledEvent> {
        let mut out: Vec<EnabledEvent> = Vec::new();
        if snapshot.status == Status::Done {
            return out;
        }
        let probe = Event::default();
        let tracer = Tracer {
            m: self,
            snap: snapshot,
            event: &probe,
            guards,
        };
        for (t, tr) in self.trans.iter().enumerate() {
            let Some(text) = &tr.event_text else { continue };
            if !snapshot.is_active(tr.source) {
                continue;
            }
            let depends = tr.guard.as_ref().is_some_and(reads_event);
            let holds = match (&tr.def_guard, &tr.guard) {
                (Some(w), Some(c)) => depends || tracer.trace(w, c).result == Some(true),
                _ => true,
            };
            if !holds {
                continue;
            }
            let reference = self.trans_ref(t as TransId);
            for descriptor in text.split_whitespace() {
                match out.iter_mut().find(|e| e.event == descriptor) {
                    Some(e) => {
                        e.transitions.push(reference.clone());
                        e.depends_on_event |= depends;
                    }
                    None => out.push(EnabledEvent {
                        event: descriptor.to_string(),
                        transitions: alloc::vec![reference.clone()],
                        depends_on_event: depends,
                    }),
                }
            }
        }
        out
    }
}

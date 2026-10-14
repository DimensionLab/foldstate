// SPDX-License-Identifier: Apache-2.0
#![doc = include_str!("../README.md")]

#[cfg(feature = "stateright")]
pub mod stateright;

use std::collections::{BTreeMap, VecDeque};

use foldstate::{
    Event, Guard, Guards, Log, LogError, MAIN, Machine, NoGuards, Snapshot, StepError,
    TransitionRef, Value,
};
use serde::Serialize;

/// A sequence of events from the start of a machine.
///
/// Every finding comes with one: the shortest sequence the exploration found that leads to
/// it. Turn it into a log to step through it.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Trace {
    pub events: Vec<Event>,
}

impl Trace {
    /// Replays the trace into a new log. The last entry is the state the finding is about.
    pub fn to_log(&self, machine: &Machine, guards: &dyn Guards) -> Result<Log, LogError> {
        let mut log = Log::start_with(machine, Value::Null, guards)?;
        for e in &self.events {
            log.append_with(machine, MAIN, e.clone(), guards)?;
        }
        Ok(log)
    }
}

/// A predicate over a machine and one of its snapshots.
pub type Predicate<'a> = Box<dyn Fn(&Machine, &Snapshot) -> bool + 'a>;

/// A property that must hold in every reachable state.
pub enum Invariant<'a> {
    /// A guard in the definition's own guard language, written against the root
    /// (`{"in": "#a.b"}`, `not`, `all`, `any`, `cmp`).
    Guard(Guard),
    /// Any predicate over the machine and a snapshot.
    Fn(Predicate<'a>),
}

/// How far to explore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    /// Stop after this many distinct snapshots.
    pub max_states: usize,
    /// Do not follow event sequences longer than this.
    pub max_depth: usize,
}

impl Default for Bounds {
    fn default() -> Self {
        Bounds {
            max_states: 100_000,
            max_depth: usize::MAX,
        }
    }
}

/// A reachable step that fails: a livelock or an action that cannot be evaluated.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Failure {
    /// The events up to and including the one that fails.
    pub trace: Trace,
    pub error: String,
}

/// A reachable state in which an invariant does not hold.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Violation {
    pub invariant: String,
    pub trace: Trace,
}

/// What an exploration found.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct Report {
    /// Distinct snapshots reached.
    pub states: usize,
    /// Steps taken between them.
    pub steps: usize,
    /// False when a bound stopped the exploration: findings are then valid, but "nothing
    /// found" proves nothing, and the lists of unreached states and transitions may name
    /// things that are reachable further on.
    pub complete: bool,
    /// Paths of states that were never active.
    pub unreached_states: Vec<String>,
    /// Transitions that were never taken.
    pub unused_transitions: Vec<TransitionRef>,
    /// States the machine cannot leave: not done, and no event of the alphabet and no
    /// pending timer causes a transition. One trace per distinct configuration.
    pub deadlocks: Vec<Trace>,
    pub failures: Vec<Failure>,
    pub violations: Vec<Violation>,
    /// Whether some reachable state is done.
    pub can_finish: bool,
    /// When the machine can finish: a reachable state from which it no longer can, if there
    /// is one.
    pub cannot_finish_from: Option<Trace>,
    /// The shortest event sequence that makes each state active, by state path.
    pub paths_to_states: BTreeMap<String, Trace>,
    /// The shortest event sequence whose last event takes each transition, keyed
    /// `<source path>[<index>]`.
    pub paths_to_transitions: BTreeMap<String, Trace>,
}

impl Report {
    /// True when the exploration was complete and found no deadlock, failure, violation or
    /// state from which the machine cannot finish.
    pub fn is_clean(&self) -> bool {
        self.complete
            && self.deadlocks.is_empty()
            && self.failures.is_empty()
            && self.violations.is_empty()
            && self.cannot_finish_from.is_none()
    }
}

struct Node {
    snapshot: Snapshot,
    /// The node and event this one was first reached by.
    from: Option<(usize, Event)>,
    depth: usize,
    next: Vec<usize>,
}

/// Explores the states a machine can reach.
///
/// The exploration is breadth-first over snapshots, trying every event of the alphabet and
/// the event of every pending timer in each. Two snapshots are the same state when their
/// serialised forms are equal, so a context that grows without bound (a counter) makes the
/// state space infinite: bound the exploration or keep such data out of the checked model.
pub struct Explorer<'a> {
    machine: &'a Machine,
    guards: &'a dyn Guards,
    events: Vec<Event>,
    invariants: Vec<(String, Invariant<'a>)>,
    bounds: Bounds,
    input: Value,
}

impl<'a> Explorer<'a> {
    /// An explorer whose alphabet is every event name the definition names exactly, each
    /// without data.
    pub fn new(machine: &'a Machine) -> Self {
        Explorer {
            machine,
            guards: &NoGuards,
            events: machine.event_names().into_iter().map(Event::new).collect(),
            invariants: Vec::new(),
            bounds: Bounds::default(),
            input: Value::Null,
        }
    }

    /// Adds an event to the alphabet: one with data that a guard reads, or one that only a
    /// pattern (`a.*`) matches.
    pub fn event(mut self, event: Event) -> Self {
        if !self.events.contains(&event) {
            self.events.push(event);
        }
        self
    }

    /// Replaces the alphabet.
    pub fn events(mut self, events: Vec<Event>) -> Self {
        self.events = events;
        self
    }

    pub fn guards(mut self, guards: &'a dyn Guards) -> Self {
        self.guards = guards;
        self
    }

    /// The start input of the machine.
    pub fn input(mut self, input: Value) -> Self {
        self.input = input;
        self
    }

    pub fn bounds(mut self, bounds: Bounds) -> Self {
        self.bounds = bounds;
        self
    }

    /// Requires a guard to hold in every reachable state.
    pub fn always(mut self, name: impl Into<String>, guard: Guard) -> Self {
        self.invariants.push((name.into(), Invariant::Guard(guard)));
        self
    }

    /// Requires a guard to hold in no reachable state.
    pub fn never(self, name: impl Into<String>, guard: Guard) -> Self {
        self.always(name, Guard::Not(Box::new(guard)))
    }

    /// Requires a predicate to hold in every reachable state.
    pub fn always_fn(
        mut self,
        name: impl Into<String>,
        f: impl Fn(&Machine, &Snapshot) -> bool + 'a,
    ) -> Self {
        self.invariants
            .push((name.into(), Invariant::Fn(Box::new(f))));
        self
    }

    fn key(&self, snapshot: &Snapshot) -> String {
        serde_json::to_string(&self.machine.to_wire(snapshot)).unwrap_or_default()
    }

    fn trace(nodes: &[Node], mut i: usize, last: Option<&Event>) -> Trace {
        let mut events: Vec<Event> = last.cloned().into_iter().collect();
        while let Some((parent, event)) = &nodes[i].from {
            events.push(event.clone());
            i = *parent;
        }
        events.reverse();
        Trace { events }
    }

    /// Explores and reports. Fails only when the machine cannot be started.
    pub fn run(&self) -> Result<Report, StepError> {
        let m = self.machine;
        let started = m.start_with(self.input.clone(), self.guards)?;
        let mut report = Report {
            complete: true,
            ..Report::default()
        };
        for path in started.change.visited(m) {
            report
                .paths_to_states
                .insert(path.to_string(), Trace::default());
        }
        for microstep in started.change.microsteps(m) {
            for t in microstep {
                report
                    .paths_to_transitions
                    .insert(format!("{}[{}]", t.source, t.index), Trace::default());
            }
        }
        let start = started.snapshot;
        let mut seen: BTreeMap<String, usize> = BTreeMap::new();
        let mut nodes: Vec<Node> = Vec::new();
        let mut queue: VecDeque<usize> = VecDeque::new();
        seen.insert(self.key(&start), 0);
        nodes.push(Node {
            snapshot: start,
            from: None,
            depth: 0,
            next: Vec::new(),
        });
        queue.push_back(0);
        let mut deadlocked: Vec<Vec<String>> = Vec::new();
        let mut violated: Vec<&str> = Vec::new();

        while let Some(i) = queue.pop_front() {
            let snapshot = nodes[i].snapshot.clone();
            for path in m.configuration(&snapshot) {
                if !report.paths_to_states.contains_key(path) {
                    report
                        .paths_to_states
                        .insert(path.to_string(), Self::trace(&nodes, i, None));
                }
            }
            for (name, invariant) in &self.invariants {
                let holds = match invariant {
                    Invariant::Guard(g) => m.check_guard(&snapshot, g, self.guards) == Some(true),
                    Invariant::Fn(f) => f(m, &snapshot),
                };
                if !holds && !violated.contains(&name.as_str()) {
                    violated.push(name);
                    report.violations.push(Violation {
                        invariant: name.clone(),
                        trace: Self::trace(&nodes, i, None),
                    });
                }
            }
            if snapshot.is_done() {
                report.can_finish = true;
                continue;
            }
            if nodes[i].depth >= self.bounds.max_depth {
                report.complete = false;
                continue;
            }
            // The alphabet, then whatever timers are pending here.
            let mut events: Vec<Event> = self.events.clone();
            for timer in snapshot.timers().values() {
                if !events.iter().any(|e| e.name == timer.event) {
                    events.push(Event::new(timer.event.clone()));
                }
            }
            let mut moved = false;
            for event in &events {
                match m.step_with(&snapshot, event, self.guards) {
                    Ok(step) => {
                        if !step.change.handled {
                            continue;
                        }
                        moved = true;
                        report.steps += 1;
                        // States passed through within the step count as reached.
                        for path in step.change.visited(m) {
                            if !report.paths_to_states.contains_key(path) {
                                report
                                    .paths_to_states
                                    .insert(path.to_string(), Self::trace(&nodes, i, Some(event)));
                            }
                        }
                        for microstep in step.change.microsteps(m) {
                            for t in microstep {
                                let key = format!("{}[{}]", t.source, t.index);
                                report
                                    .paths_to_transitions
                                    .entry(key)
                                    .or_insert_with(|| Self::trace(&nodes, i, Some(event)));
                            }
                        }
                        let key = self.key(&step.snapshot);
                        let j = match seen.get(&key) {
                            Some(&j) => j,
                            None => {
                                if nodes.len() >= self.bounds.max_states {
                                    report.complete = false;
                                    continue;
                                }
                                let j = nodes.len();
                                seen.insert(key, j);
                                nodes.push(Node {
                                    snapshot: step.snapshot,
                                    from: Some((i, event.clone())),
                                    depth: nodes[i].depth + 1,
                                    next: Vec::new(),
                                });
                                queue.push_back(j);
                                j
                            }
                        };
                        nodes[i].next.push(j);
                    }
                    Err(StepError::Rejected { .. }) | Err(StepError::Done { .. }) => {}
                    Err(error) => {
                        moved = true;
                        let text = error.to_string();
                        if !report.failures.iter().any(|f| f.error == text) {
                            report.failures.push(Failure {
                                trace: Self::trace(&nodes, i, Some(event)),
                                error: text,
                            });
                        }
                    }
                }
            }
            if !moved {
                let configuration: Vec<String> = m
                    .configuration(&snapshot)
                    .into_iter()
                    .map(String::from)
                    .collect();
                if !deadlocked.contains(&configuration) {
                    deadlocked.push(configuration);
                    report.deadlocks.push(Self::trace(&nodes, i, None));
                }
            }
        }

        report.states = nodes.len();
        for s in m.states() {
            if !s.path.is_empty()
                && s.kind != foldstate::Kind::History
                && !report.paths_to_states.contains_key(s.path)
            {
                report.unreached_states.push(s.path.to_string());
            }
        }
        for t in m.transitions() {
            let key = format!("{}[{}]", t.reference.source, t.reference.index);
            if !report.paths_to_transitions.contains_key(&key) {
                report.unused_transitions.push(t.reference);
            }
        }

        // From which states can the machine still finish? Work backwards from the done ones.
        if report.can_finish && report.complete {
            let mut back: Vec<Vec<usize>> = vec![Vec::new(); nodes.len()];
            for (i, n) in nodes.iter().enumerate() {
                for &j in &n.next {
                    back[j].push(i);
                }
            }
            let mut can = vec![false; nodes.len()];
            let mut work: Vec<usize> = (0..nodes.len())
                .filter(|&i| nodes[i].snapshot.is_done())
                .collect();
            for &i in &work {
                can[i] = true;
            }
            while let Some(i) = work.pop() {
                for &p in &back[i] {
                    if !can[p] {
                        can[p] = true;
                        work.push(p);
                    }
                }
            }
            // Nodes are in breadth-first order, so the first is one of the nearest.
            if let Some(i) = can.iter().position(|c| !c) {
                report.cannot_finish_from = Some(Self::trace(&nodes, i, None));
            }
        }
        Ok(report)
    }
}

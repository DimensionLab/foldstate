// SPDX-License-Identifier: Apache-2.0
//! A machine as a [`stateright`] model, for its checkers and its explorer.
//!
//! ```
//! use foldstate::{Definition, Machine};
//! use foldstate_check::stateright::Chart;
//! use stateright::{Checker, Model};
//!
//! let definition: Definition = serde_json::from_str(r##"{
//!   "schema": "foldstate/1", "id": "door",
//!   "root": {"id": "door", "states": [
//!     {"id": "closed", "on": [{"event": "open", "target": ["opened"]}]},
//!     {"id": "opened", "on": [{"event": "close", "target": ["closed"]}]}
//!   ]}
//! }"##)?;
//! let chart = Chart::new(Machine::compile(&definition)?)
//!     .sometimes("can open", |chart, state| chart.machine().is_in(state.snapshot(), "opened"))
//!     .always("never both", |chart, state| chart.machine().leaves(state.snapshot()).len() == 1);
//! let checker = chart.checker().spawn_bfs().join();
//! checker.assert_properties();
//! assert_eq!(checker.unique_state_count(), 2);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::hash::{Hash, Hasher};

use foldstate::{Event, Machine, Snapshot, Value};
use stateright::{Expectation, Model, Property};

/// A snapshot that can be hashed and compared: by its serialised form.
#[derive(Debug, Clone)]
pub struct ChartState {
    snapshot: Snapshot,
    key: String,
}

impl ChartState {
    pub fn snapshot(&self) -> &Snapshot {
        &self.snapshot
    }
}

impl PartialEq for ChartState {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for ChartState {}

impl Hash for ChartState {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key.hash(state);
    }
}

type Condition = fn(&Chart, &ChartState) -> bool;

#[derive(Clone, Copy)]
enum Expect {
    Always,
    Sometimes,
    Eventually,
}

/// A machine with the events to try and the properties to check. Host guards are not
/// available here: a named guard counts as false.
#[derive(Clone)]
pub struct Chart {
    machine: Machine,
    events: Vec<Event>,
    input: Value,
    properties: Vec<(Expect, &'static str, Condition)>,
}

impl Chart {
    /// The alphabet is every event name the definition names exactly, without data; pending
    /// timers are tried as well.
    pub fn new(machine: Machine) -> Self {
        let events = machine.event_names().into_iter().map(Event::new).collect();
        Chart {
            machine,
            events,
            input: Value::Null,
            properties: Vec::new(),
        }
    }

    pub fn machine(&self) -> &Machine {
        &self.machine
    }

    /// Adds an event to the alphabet.
    pub fn event(mut self, event: Event) -> Self {
        if !self.events.contains(&event) {
            self.events.push(event);
        }
        self
    }

    /// The start input of the machine.
    pub fn input(mut self, input: Value) -> Self {
        self.input = input;
        self
    }

    /// Must hold in every reachable state.
    pub fn always(mut self, name: &'static str, condition: Condition) -> Self {
        self.properties.push((Expect::Always, name, condition));
        self
    }

    /// Must hold in some reachable state.
    pub fn sometimes(mut self, name: &'static str, condition: Condition) -> Self {
        self.properties.push((Expect::Sometimes, name, condition));
        self
    }

    /// Must hold at some point on every path.
    pub fn eventually(mut self, name: &'static str, condition: Condition) -> Self {
        self.properties.push((Expect::Eventually, name, condition));
        self
    }

    fn state(&self, snapshot: Snapshot) -> ChartState {
        let key = serde_json::to_string(&self.machine.to_wire(&snapshot)).unwrap_or_default();
        ChartState { snapshot, key }
    }
}

impl Model for Chart {
    type State = ChartState;
    type Action = Event;

    fn init_states(&self) -> Vec<ChartState> {
        match self.machine.start(self.input.clone()) {
            Ok(step) => vec![self.state(step.snapshot)],
            Err(_) => Vec::new(),
        }
    }

    fn actions(&self, state: &ChartState, actions: &mut Vec<Event>) {
        if state.snapshot.is_done() {
            return;
        }
        actions.extend(self.events.iter().cloned());
        for timer in state.snapshot.timers().values() {
            if !actions.iter().any(|e| e.name == timer.event) {
                actions.push(Event::new(timer.event.clone()));
            }
        }
    }

    fn next_state(&self, last: &ChartState, action: Event) -> Option<ChartState> {
        // A refused or ignored event is not a step.
        let step = self.machine.step(&last.snapshot, &action).ok()?;
        step.change.handled.then(|| self.state(step.snapshot))
    }

    fn format_action(&self, action: &Event) -> String {
        action.name.clone()
    }

    fn properties(&self) -> Vec<Property<Self>> {
        self.properties
            .iter()
            .map(|&(expect, name, condition)| Property {
                expectation: match expect {
                    Expect::Always => Expectation::Always,
                    Expect::Sometimes => Expectation::Sometimes,
                    Expect::Eventually => Expectation::Eventually,
                },
                name,
                condition,
            })
            .collect()
    }
}

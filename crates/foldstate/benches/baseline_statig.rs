// SPDX-License-Identifier: Apache-2.0
//! The same nine-state lifecycle in foldstate (a definition interpreted at run time) and in
//! statig (a state machine compiled from Rust code), one event at a time.
//!
//! statig is the upper bound: its machine is fixed when the program is compiled, it keeps no
//! record of what changed and it can inline every handler. The comparison shows what
//! interpreting a definition and recording a reversible change cost.

use std::hint::black_box;
use std::time::Instant;

use foldstate::{Definition, Event, Machine, NoGuards, Value};
use serde_json::json;
use statig::prelude::*;

#[derive(Default)]
struct Job {
    progress: f64,
}

enum Ev {
    Plan,
    Queue,
    Run,
    Progress(f64),
    Ok,
    Reset,
}

#[state_machine(initial = "State::pending()")]
impl Job {
    #[state]
    fn pending(event: &Ev) -> Outcome<State> {
        match event {
            Ev::Plan => Transition(State::waiting()),
            _ => Super,
        }
    }

    #[state]
    fn waiting(event: &Ev) -> Outcome<State> {
        match event {
            Ev::Queue => Transition(State::queued()),
            _ => Super,
        }
    }

    #[state]
    fn queued(event: &Ev) -> Outcome<State> {
        match event {
            Ev::Run => Transition(State::running()),
            _ => Super,
        }
    }

    #[state]
    fn running(&mut self, event: &Ev) -> Outcome<State> {
        match event {
            Ev::Progress(fraction) => {
                self.progress = *fraction;
                Handled
            }
            Ev::Ok => Transition(State::succeeded()),
            _ => Super,
        }
    }

    #[state]
    fn succeeded(event: &Ev) -> Outcome<State> {
        match event {
            Ev::Reset => Transition(State::pending()),
            _ => Super,
        }
    }
}

fn definition() -> Definition {
    serde_json::from_value(json!({
        "schema": "foldstate/1", "id": "job",
        "root": {"id": "job", "states": [
            {"id": "pending", "on": [{"event": "plan", "target": ["waiting"]}]},
            {"id": "waiting", "on": [{"event": "queue", "target": ["queued"]}]},
            {"id": "queued", "on": [{"event": "run", "target": ["running"]}]},
            {"id": "running", "on": [
                {"event": "progress", "actions": [{"assign": {"path": "/progress", "value": {"event": "/fraction"}}}]},
                {"event": "ok", "target": ["succeeded"]}
            ]},
            {"id": "succeeded", "on": [{"event": "reset", "target": ["pending"]}]}
        ]}
    }))
    .expect("definition")
}

fn best<F: FnMut() -> u64>(mut f: F) -> f64 {
    let mut best = f64::INFINITY;
    for _ in 0..7 {
        let start = Instant::now();
        let ops = f();
        best = best.min(start.elapsed().as_nanos() as f64 / ops as f64);
    }
    best
}

fn main() {
    const ROUNDS: u64 = 100_000;

    let cycle = [
        Ev::Plan,
        Ev::Queue,
        Ev::Run,
        Ev::Progress(0.25),
        Ev::Progress(0.75),
        Ev::Ok,
        Ev::Reset,
    ];
    let mut job = Job::default().state_machine();
    let statig = best(|| {
        for _ in 0..ROUNDS {
            for e in &cycle {
                job.handle(black_box(e));
            }
        }
        black_box(job.inner().progress);
        ROUNDS * cycle.len() as u64
    });

    let machine = Machine::compile(&definition()).expect("compiles");
    let events: Vec<Event> = vec![
        Event::new("plan"),
        Event::new("queue"),
        Event::new("run"),
        Event::new("progress").with_data(json!({"fraction": 0.25})),
        Event::new("progress").with_data(json!({"fraction": 0.75})),
        Event::new("ok"),
        Event::new("reset"),
    ];
    let mut snapshot = machine.start(Value::Null).expect("starts").snapshot;
    let foldstate = best(|| {
        for _ in 0..ROUNDS / 10 {
            for e in &events {
                black_box(
                    machine
                        .step_in_place(&mut snapshot, black_box(e), &NoGuards)
                        .expect("accepted"),
                );
            }
        }
        ROUNDS / 10 * events.len() as u64
    });

    println!("one event, five-state cycle with two progress updates");
    println!("statig 0.4 (compiled from Rust)        {statig:>8.1} ns");
    println!("foldstate (definition from JSON)       {foldstate:>8.1} ns");
    println!(
        "ratio                                  {:>8.0}x",
        foldstate / statig
    );
}

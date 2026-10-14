// SPDX-License-Identifier: Apache-2.0
//! A definition and a list of events: every step leaves a legal configuration, a refused
//! event changes nothing, every change can be undone and redone, and the log of the run
//! verifies and reads back.
#![no_main]

use foldstate::{Definition, Event, Limits, Log, MAIN, Machine, NoGuards, Value, WireLog};
use libfuzzer_sys::fuzz_target;
use serde::Deserialize;

#[derive(Deserialize)]
struct Input {
    definition: Definition,
    #[serde(default)]
    events: Vec<Event>,
}

fuzz_target!(|data: &[u8]| {
    let Ok(mut input) = serde_json::from_slice::<Input>(data) else {
        return;
    };
    // Keep one input cheap: a bounded machine and a bounded run.
    input.definition.max_microsteps = input.definition.max_microsteps.min(64);
    input.events.truncate(64);
    let limits = Limits {
        max_states: 500,
        max_transitions: 2_000,
        max_depth: 16,
    };
    let Ok(machine) = Machine::compile_with(&input.definition, &limits) else {
        return;
    };
    let Ok(mut log) = Log::start(&machine, Value::Null) else {
        return;
    };
    log.set_checkpoint_interval(4);
    for event in input.events {
        let before = log.head(MAIN).expect("head").clone();
        let explained = machine.explain(&before, &event, &NoGuards);
        match log.append(&machine, MAIN, event) {
            Ok(entry) => {
                let change = entry.change.clone();
                let after = log.head(MAIN).expect("head").clone();
                machine
                    .from_wire(&machine.to_wire(&after))
                    .expect("a step leaves a legal configuration");
                let mut undone = after.clone();
                change.undo(&mut undone).expect("a change can be undone");
                assert_eq!(undone, before);
                change.redo(&mut undone).expect("and redone");
                assert_eq!(undone, after);
                if change.handled {
                    assert_eq!(explained.outcome, foldstate::Outcome::Handled);
                }
            }
            Err(_) => assert_eq!(log.head(MAIN).expect("head"), &before),
        }
    }
    log.verify(&machine).expect("a log the engine wrote verifies");
    let head = log.head_seq(MAIN).expect("head");
    assert_eq!(&log.state_at(MAIN, head).expect("state"), log.head(MAIN).expect("head"));
    if head > 1 {
        let branch = log.fork(MAIN, head / 2 + 1).expect("fork");
        let _ = log.restore(&machine, branch, 1);
        let _ = log.diff(&machine, (MAIN, head), (branch, 1));
    }
    let wire = log.to_wire(&machine).expect("wire");
    let text = serde_json::to_string(&wire).expect("a log serialises");
    let parsed: WireLog = serde_json::from_str(&text).expect("and parses back");
    let read = Log::from_wire(&machine, &parsed).expect("and is accepted");
    read.verify(&machine).expect("and verifies");
});

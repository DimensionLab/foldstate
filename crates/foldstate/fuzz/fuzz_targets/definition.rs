// SPDX-License-Identifier: Apache-2.0
//! Any bytes as a definition: parsing and compiling must refuse or succeed, never panic, and
//! a definition that compiles must start, list its graph and survive its own serialisation.
#![no_main]

use foldstate::{Definition, Limits, Machine, NoGuards, Value};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(definition) = serde_json::from_slice::<Definition>(data) else {
        return;
    };
    let limits = Limits {
        max_states: 2_000,
        max_transitions: 8_000,
        max_depth: 32,
    };
    let Ok(machine) = Machine::compile_with(&definition, &limits) else {
        return;
    };
    let text = serde_json::to_string(&definition).expect("a definition serialises");
    let again: Definition = serde_json::from_str(&text).expect("and parses back");
    let same = Machine::compile_with(&again, &limits).expect("and compiles again");
    assert_eq!(same.hash(), machine.hash());
    let _ = machine.states().count() + machine.transitions().count();
    if let Ok(step) = machine.start(Value::Null) {
        let wire = machine.to_wire(&step.snapshot);
        let read = machine.from_wire(&wire).expect("a snapshot the machine made is legal");
        assert_eq!(read, step.snapshot);
        let _ = machine.enabled(&step.snapshot, &NoGuards);
    }
});

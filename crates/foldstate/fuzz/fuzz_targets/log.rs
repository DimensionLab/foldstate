// SPDX-License-Identifier: Apache-2.0
//! Any bytes as a serialised log of a fixed machine: reading, verifying, walking and
//! replaying it must refuse or succeed, never panic.
#![no_main]

use std::sync::OnceLock;

use foldstate::{Definition, Log, Machine, WireLog, WireSnapshot};
use libfuzzer_sys::fuzz_target;

const DEFINITION: &str = r##"{
  "schema": "foldstate/1", "id": "fuzz", "context": {"n": 0},
  "root": {"id": "fuzz", "kind": "parallel", "states": [
    {"id": "a", "states": [
      {"id": "a1", "on": [{"event": "go", "target": ["a2"], "actions": [
        {"assign": {"path": "/n", "value": {"add": [{"ctx": "/n"}, {"lit": 1}]}}},
        {"emit": {"name": "went"}}]}],
       "after": [{"delay_ms": 10, "target": ["a2"]}]},
      {"id": "a2", "invoke": [{"name": "job"}], "on": [{"event": "back", "target": ["h"]}]},
      {"id": "h", "kind": "history"}
    ]},
    {"id": "b", "states": [
      {"id": "b1", "on": [{"event": "go", "target": ["b2"]}]},
      {"id": "b2", "kind": "final"}
    ]}
  ]}
}"##;

fn machine() -> &'static Machine {
    static MACHINE: OnceLock<Machine> = OnceLock::new();
    MACHINE.get_or_init(|| {
        let definition: Definition = serde_json::from_str(DEFINITION).expect("definition");
        Machine::compile(&definition).expect("compiles")
    })
}

fuzz_target!(|data: &[u8]| {
    let Ok(mut wire) = serde_json::from_slice::<WireLog>(data) else {
        return;
    };
    // Following entries without the definition accepts any well-formed entry or refuses it.
    let mut snapshot = WireSnapshot::blank(wire.machine.clone());
    for branch in &wire.branches {
        for entry in &branch.entries {
            if snapshot.redo(entry).is_err() {
                break;
            }
        }
    }
    // With the definition: whatever the hash says, try it as a log of this machine.
    let machine = machine();
    wire.machine = machine.hash().to_string();
    let Ok(log) = Log::from_wire(machine, &wire) else {
        return;
    };
    let _ = log.verify(machine);
    for branch in log.branches() {
        if let Ok(mut cursor) = log.cursor(branch.id, branch.head.max(1)) {
            while cursor.back().is_ok() {}
            while cursor.forward().is_ok() {}
        }
        let _ = log.state_at(branch.id, branch.head / 2 + 1);
    }
    let _ = log.to_wire(machine);
});

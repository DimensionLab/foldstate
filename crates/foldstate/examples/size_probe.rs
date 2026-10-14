// SPDX-License-Identifier: Apache-2.0
//! Uses the whole engine once, so that a release build for `wasm32-unknown-unknown` shows
//! how much code the engine adds to a binary:
//!
//! ```text
//! cargo build -p foldstate --release --example size_probe --target wasm32-unknown-unknown
//! cargo build -p foldstate --release --example size_baseline --target wasm32-unknown-unknown
//! ```
//!
//! `size_baseline` only parses and prints JSON; the difference is the engine.

use std::hint::black_box;

use foldstate::{Definition, Event, Log, MAIN, Machine, NoGuards, WireLog};

fn main() {
    let text = black_box(String::from(
        r#"{"schema":"foldstate/1","id":"m","root":{"id":"m","states":[{"id":"a","on":[{"event":"go","target":["b"]}]},{"id":"b"}]}}"#,
    ));
    let Ok(definition) = serde_json::from_str::<Definition>(&text) else {
        return;
    };
    let Ok(machine) = Machine::compile(&definition) else {
        return;
    };
    let Ok(mut log) = Log::start(&machine, serde_json::Value::Null) else {
        return;
    };
    let event = Event::new(black_box(String::from("go")));
    let explanation = machine.explain(log.head(MAIN).expect("head"), &event, &NoGuards);
    let _ = log.append(&machine, MAIN, event);
    let branch = log.fork(MAIN, 1).unwrap_or(MAIN);
    let _ = log.restore(&machine, MAIN, 1);
    let _ = log.verify(&machine);
    let _ = log.diff(&machine, (MAIN, 1), (branch, 1));
    let enabled = machine.enabled(log.head(MAIN).expect("head"), &NoGuards);
    let wire = log.to_wire(&machine).expect("wire");
    let json = serde_json::to_string(&(wire, explanation, enabled)).unwrap_or_default();
    let back: Result<(WireLog, serde_json::Value, serde_json::Value), _> =
        serde_json::from_str(&json);
    if let Ok((wire, _, _)) = back {
        let _ = black_box(Log::from_wire(&machine, &wire));
    }
    black_box(json);
}

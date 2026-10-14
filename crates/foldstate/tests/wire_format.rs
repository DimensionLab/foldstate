// SPDX-License-Identifier: Apache-2.0
//! The serialised forms are frozen: for a fixed machine and run, the definition, a snapshot
//! and the log must serialise to exactly the bytes in `tests/golden/`, and those files must
//! read back. A change here is a change of format: bump the `schema` number
//! (`docs/semantics.md`, "Formats and versions") and record new files with
//! `FOLDSTATE_RECORD_GOLDEN=1`.

use foldstate::{
    Definition, Event, Log, MAIN, Machine, StateDef, TransitionDef, Value, WireLog, WireSnapshot,
};
use serde_json::json;

fn definition() -> Definition {
    serde_json::from_value(json!({
        "schema": "foldstate/1", "id": "golden", "context": {"n": 0, "note": null},
        "ignore": ["noise.*"],
        "root": {"id": "golden", "kind": "parallel", "states": [
            {"id": "job", "initial": "idle", "states": [
                {"id": "h", "kind": "history", "deep": true, "default": ["idle"]},
                {"id": "idle", "on": [{"event": "start", "target": ["busy"],
                    "guard": {"all": [{"not": {"in": "#gate.closed"}},
                                      {"cmp": {"left": {"ctx": "/n"}, "op": "lt", "right": {"lit": 3}}},
                                      {"named": {"name": "allowed", "params": {"who": "anyone"}}}]},
                    "actions": [{"assign": {"path": "/n", "value": {"add": [{"ctx": "/n"}, {"lit": 1}]}}},
                                {"assign": {"path": "/note", "value": {"event": "/note"}}},
                                {"emit": {"name": "started", "params": {"ctx": "/n"}}},
                                {"raise": {"event": "internal.started", "data": {"lit": {"k": 1}}}}]}]},
                {"id": "busy", "tags": ["working"], "meta": {"label": "Busy"},
                 "invoke": [{"name": "work", "params": {"event": "/note"}}],
                 "after": [{"delay_ms": 5000, "target": ["idle"], "actions": [{"cancel": {"id": "extra"}}]}],
                 "entry": [{"schedule": {"id": "extra", "event": "tick", "delay_ms": 100}}],
                 "on": [{"event": "pause", "target": ["#gate.closed", "#job.idle"]},
                        {"event": "tick", "internal": true},
                        {"event": "finish", "target": ["done"]}]},
                {"id": "done", "kind": "final"}
            ]},
            {"id": "gate", "states": [
                {"id": "open"},
                {"id": "closed", "on": [{"event": "resume", "target": ["#job.h", "open"]},
                                         {"target": ["open"], "guard": {"any": [{"cmp": {"left": {"sub": [{"mul": [{"ctx": "/n"}, {"lit": 2}]}, {"lit": 1}]}, "op": "ge", "right": {"lit": 99}}}]}}]}
            ]}
        ]}
    }))
    .unwrap()
}

fn allowed(name: &str, _: &Value, _: &Value, _: &Event) -> Option<bool> {
    (name == "allowed").then_some(true)
}

fn check(name: &str, text: &str) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    if std::env::var("FOLDSTATE_RECORD_GOLDEN").is_ok() {
        std::fs::write(&path, text).unwrap();
        return;
    }
    let golden = std::fs::read_to_string(&path).unwrap_or_default();
    assert_eq!(text, golden, "{name} changed: this is a change of format");
}

#[test]
fn serialised_forms_are_frozen() {
    let def = definition();
    let machine = Machine::compile(&def).unwrap();
    let mut log = Log::start_with(&machine, json!({"note": "from input"}), &allowed).unwrap();
    let send = |log: &mut Log, branch, name: &str, data: Value, at: i64| {
        log.append_with(
            &machine,
            branch,
            Event::new(name).with_data(data).at(at),
            &allowed,
        )
        .map(|e| e.seq)
    };
    send(&mut log, MAIN, "start", json!({"note": "first"}), 1_000).unwrap();
    send(&mut log, MAIN, "noise.hum", Value::Null, 1_500).unwrap();
    send(&mut log, MAIN, "pause", Value::Null, 2_000).unwrap();
    let at_pause = log.head_seq(MAIN).unwrap();
    send(&mut log, MAIN, "resume", Value::Null, 3_000).unwrap();
    let fork = log.fork(MAIN, at_pause).unwrap();
    send(&mut log, fork, "start", json!({"note": "second"}), 4_000).unwrap_err();
    send(&mut log, fork, "resume", Value::Null, 4_500).unwrap();
    send(&mut log, MAIN, "start", json!({"note": "third"}), 5_000).unwrap();
    send(&mut log, MAIN, "tick", Value::Null, 5_100).unwrap();
    log.restore(&machine, MAIN, 2).unwrap();
    send(&mut log, MAIN, "finish", Value::Null, 6_000).unwrap();
    log.verify_with(&machine, &allowed).unwrap();
    log.verify(&machine).unwrap();

    let pretty = |v: &dyn erased::Json| v.pretty();
    check("definition.json", &pretty(&def));
    check("definition.hash", &format!("{}\n", machine.hash()));
    let snapshot = machine.to_wire(&log.state_at(MAIN, 2).unwrap());
    check("snapshot.json", &pretty(&snapshot));
    let wire = log.to_wire(&machine).unwrap();
    check("log.json", &pretty(&wire));

    // The golden files read back to the same values.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let read = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap();
    let golden_def: Definition = serde_json::from_str(&read("definition.json")).unwrap();
    assert_eq!(golden_def, def);
    assert_eq!(
        Machine::compile(&golden_def).unwrap().hash(),
        read("definition.hash").trim()
    );
    let golden_snapshot: WireSnapshot = serde_json::from_str(&read("snapshot.json")).unwrap();
    assert_eq!(
        machine.from_wire(&golden_snapshot).unwrap(),
        log.state_at(MAIN, 2).unwrap()
    );
    let golden_log: WireLog = serde_json::from_str(&read("log.json")).unwrap();
    let read_log = Log::from_wire(&machine, &golden_log).unwrap();
    read_log.verify(&machine).unwrap();
    assert_eq!(read_log.head(MAIN).unwrap(), log.head(MAIN).unwrap());

    // The builder gives the same definition as its JSON.
    let built = Definition::new(
        "b",
        StateDef::compound(
            "b",
            vec![StateDef::atomic("x").on("go", "y"), StateDef::atomic("y")],
        ),
    );
    let parsed: Definition = serde_json::from_value(json!({
        "schema": "foldstate/1", "id": "b",
        "root": {"id": "b", "kind": "compound", "states": [
            {"id": "x", "on": [{"event": "go", "target": ["y"]}]}, {"id": "y"}]}
    }))
    .unwrap();
    assert_eq!(built, parsed);
    let _ = TransitionDef::always("y");
}

mod erased {
    /// Pretty JSON with a trailing newline, for any serialisable value.
    pub trait Json {
        fn pretty(&self) -> String;
    }

    impl<T: serde::Serialize> Json for T {
        fn pretty(&self) -> String {
            serde_json::to_string_pretty(self).unwrap() + "\n"
        }
    }
}

/// A float in the context survives being written and read: the log still verifies. (Found
/// by fuzzing: the JSON reader needs its exact mode for this.)
#[test]
fn floats_survive_the_round_trip() {
    let def: Definition = serde_json::from_value(json!({
        "schema": "foldstate/1", "id": "f", "context": {"n": 1.2999999999999998e93},
        "root": {"id": "f", "states": [{"id": "a", "on": [{"event": "add", "actions": [
            {"assign": {"path": "/n", "value": {"add": [{"ctx": "/n"}, {"lit": 1e92}]}}}
        ]}]}]}
    }))
    .unwrap();
    let machine = Machine::compile(&def).unwrap();
    let mut log = Log::start(&machine, Value::Null).unwrap();
    for _ in 0..50 {
        log.append(&machine, MAIN, Event::new("add")).unwrap();
    }
    let text = serde_json::to_string(&log.to_wire(&machine).unwrap()).unwrap();
    let read = Log::from_wire(&machine, &serde_json::from_str::<WireLog>(&text).unwrap()).unwrap();
    read.verify(&machine).unwrap();
    assert_eq!(read.head(MAIN).unwrap(), log.head(MAIN).unwrap());
}

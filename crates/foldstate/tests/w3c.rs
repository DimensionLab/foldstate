// SPDX-License-Identifier: Apache-2.0
//! The W3C SCXML 1.0 implementation report tests that foldstate can express.
//!
//! Opt-in, because the tests are downloaded from w3.org and not kept in the repository:
//!
//! ```text
//! python3 crates/foldstate/tests/w3c/fetch.py
//! FOLDSTATE_W3C=1 cargo test -p foldstate --test w3c -- --nocapture
//! ```
//!
//! `fetch.py` converts each test to a definition, or records why it does not apply (the
//! construct foldstate does not have). This test plays the host: it delivers what a state
//! sends to its own machine, in order, delayed sends in the order they fall due, and stops
//! when the machine reaches the `pass` or `fail` state. The outcome of every test is
//! compared with `tests/w3c/expected.json`.

use std::collections::{BTreeMap, VecDeque};

use foldstate::{Definition, Effect, Event, Machine, Value};
use serde::Deserialize;

#[derive(Deserialize)]
struct Case {
    id: String,
    conformance: String,
    not_applicable: Option<String>,
    definition: Option<Value>,
}

/// Runs a converted test to its end.
fn run(definition: &Value) -> String {
    let def: Definition = match serde_json::from_value(definition.clone()) {
        Ok(d) => d,
        Err(e) => return format!("refused: {e}"),
    };
    let machine = match Machine::compile(&def) {
        Ok(m) => m,
        Err(e) => return format!("refused: {e}"),
    };
    let step = match machine.start(Value::Null) {
        Ok(s) => s,
        Err(e) => return format!("error: {e}"),
    };
    let mut snapshot = step.snapshot;
    let mut now = 0.0f64;
    let mut immediate: VecDeque<String> = VecDeque::new();
    // (due, order, send id, event)
    let mut delayed: Vec<(f64, u64, Option<String>, String)> = Vec::new();
    let mut order = 0u64;
    let mut deliver =
        |effects: &[Effect],
         now: f64,
         immediate: &mut VecDeque<String>,
         delayed: &mut Vec<(f64, u64, Option<String>, String)>| {
            for fx in effects {
                let Effect::Emit { name, params } = fx else {
                    continue;
                };
                match name.as_str() {
                    "send" => {
                        let event = params["event"].as_str().unwrap_or_default().to_string();
                        let delay = params["delay"].as_f64().unwrap_or(0.0);
                        if delay == 0.0 {
                            immediate.push_back(event);
                        } else {
                            order += 1;
                            delayed.push((
                                now + delay,
                                order,
                                params["id"].as_str().map(String::from),
                                event,
                            ));
                        }
                    }
                    "cancel" => delayed.retain(|d| d.2.as_deref() != params["id"].as_str()),
                    _ => {}
                }
            }
        };
    deliver(&step.change.effects, now, &mut immediate, &mut delayed);
    for _ in 0..10_000 {
        if snapshot.is_done() {
            break;
        }
        let event = match immediate.pop_front() {
            Some(e) => e,
            None => {
                let Some(next) = delayed
                    .iter()
                    .enumerate()
                    .min_by(|a, b| (a.1.0, a.1.1).partial_cmp(&(b.1.0, b.1.1)).expect("finite"))
                    .map(|(i, _)| i)
                else {
                    break;
                };
                let (due, _, _, event) = delayed.remove(next);
                now = due;
                event
            }
        };
        match machine.step(&snapshot, &Event::new(event)) {
            Ok(step) => {
                deliver(&step.change.effects, now, &mut immediate, &mut delayed);
                snapshot = step.snapshot;
            }
            Err(e) => return format!("error: {e}"),
        }
    }
    if machine.is_in(&snapshot, "pass") {
        "pass".into()
    } else if machine.is_in(&snapshot, "fail") {
        "fail".into()
    } else {
        format!("fail: ended in {:?}", machine.leaves(&snapshot))
    }
}

#[test]
fn w3c_scxml_tests() {
    if std::env::var("FOLDSTATE_W3C").is_err() {
        println!("skipped: set FOLDSTATE_W3C=1 after running tests/w3c/fetch.py");
        return;
    }
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/w3c");
    let cases: Vec<Case> = serde_json::from_slice(
        &std::fs::read(dir.join("cache/cases.json")).expect("run tests/w3c/fetch.py first"),
    )
    .expect("cases.json");
    let mut outcomes: BTreeMap<String, String> = BTreeMap::new();
    let mut tally: BTreeMap<(String, &str), u32> = BTreeMap::new();
    for case in &cases {
        let outcome = match (&case.not_applicable, &case.definition) {
            (Some(reason), _) => format!("not applicable: {reason}"),
            (None, Some(definition)) => run(definition),
            (None, None) => "not applicable: not converted".into(),
        };
        let class = if outcome == "pass" {
            "pass"
        } else if outcome.starts_with("not applicable") {
            "not applicable"
        } else {
            "fail"
        };
        *tally.entry((case.conformance.clone(), class)).or_default() += 1;
        outcomes.insert(case.id.clone(), outcome);
    }
    for ((conformance, class), n) in &tally {
        println!("{conformance:>9} {class:<15} {n}");
    }
    for (id, outcome) in &outcomes {
        if !(outcome == "pass" || outcome.starts_with("not applicable")) {
            println!("test {id}: {outcome}");
        }
    }
    let expected_path = dir.join("expected.json");
    if std::env::var("FOLDSTATE_W3C_RECORD").is_ok() {
        std::fs::write(
            &expected_path,
            serde_json::to_string_pretty(&outcomes).unwrap() + "\n",
        )
        .unwrap();
        println!("recorded {}", expected_path.display());
        return;
    }
    let expected: BTreeMap<String, String> =
        serde_json::from_slice(&std::fs::read(&expected_path).expect("expected.json"))
            .expect("expected.json");
    let changed: Vec<_> = outcomes
        .iter()
        .filter(|(id, outcome)| expected.get(*id) != Some(outcome))
        .collect();
    assert!(
        changed.is_empty(),
        "outcomes changed (FOLDSTATE_W3C_RECORD=1 records them): {changed:?}"
    );
}

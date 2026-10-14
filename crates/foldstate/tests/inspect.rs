// SPDX-License-Identifier: Apache-2.0
//! Inspection: the graph of a machine, the events it accepts now, and why.

use foldstate::{
    Definition, Event, Guards, Kind, Machine, NoGuards, Outcome, TransitionRef, Value, Verdict,
};
use serde_json::json;

fn machine() -> Machine {
    let def: Definition = serde_json::from_value(json!({
        "schema": "foldstate/1", "id": "job", "context": {"attempt": 0},
        "root": {"id": "job", "kind": "parallel", "states": [
            {"id": "upstream", "states": [
                {"id": "running", "on": [{"event": "upstream.ok", "target": ["ok"]}]},
                {"id": "ok", "tags": ["success"]}
            ]},
            {"id": "step", "meta": {"label": "Train"}, "states": [
                {"id": "waiting", "on": [
                    {"event": "start", "guard": {"all": [
                        {"in": "#upstream.ok"},
                        {"cmp": {"left": {"ctx": "/attempt"}, "op": "lt", "right": {"lit": 3}}},
                        {"named": {"name": "has_quota"}}
                    ]}, "target": ["running"]},
                    {"event": "start", "guard": {"cmp": {"left": {"event": "/force"}, "op": "eq", "right": {"lit": true}}}, "target": ["running"]},
                    {"event": "skip", "target": ["skipped"]}
                ]},
                {"id": "running", "after": [{"delay_ms": 1000, "target": ["waiting"]}],
                 "on": [{"event": "progress.*"}]},
                {"id": "skipped", "kind": "final"}
            ]}
        ]}
    }))
    .unwrap();
    Machine::compile(&def).unwrap()
}

fn quota(yes: bool) -> impl Guards {
    move |name: &str, _: &Value, _: &Value, _: &Event| (name == "has_quota").then_some(yes)
}

#[test]
fn the_graph_can_be_enumerated() {
    let m = machine();
    let states: Vec<_> = m.states().collect();
    assert_eq!(
        states.iter().map(|s| s.path).collect::<Vec<_>>(),
        [
            "",
            "upstream",
            "upstream.running",
            "upstream.ok",
            "step",
            "step.waiting",
            "step.running",
            "step.skipped"
        ]
    );
    assert_eq!(states[0].kind, Kind::Parallel);
    assert_eq!(states[4].initial, Some("step.waiting"));
    assert_eq!(states[4].meta, &json!({"label": "Train"}));
    assert_eq!(states[3].tags, ["success"]);
    assert_eq!(states[7].kind, Kind::Final);

    let transitions: Vec<_> = m.transitions().collect();
    assert_eq!(transitions.len(), 6);
    let timeout = transitions.iter().find(|t| t.delay_ms.is_some()).unwrap();
    assert_eq!(timeout.event, Some("after.step.running:after:0"));
    assert_eq!(timeout.targets, ["step.waiting"]);
    assert_eq!(
        timeout.reference,
        TransitionRef {
            source: "step.running".into(),
            index: 1
        }
    );
    assert!(m.transition(&timeout.reference).is_some());
    assert_eq!(m.named_guards(), ["has_quota"]);
}

#[test]
fn explain_says_why_a_step_is_still_waiting() {
    let m = machine();
    let start = m.start(Value::Null).unwrap().snapshot;
    let event = Event::new("start");

    let why = m.explain(&start, &event, &quota(true));
    assert_eq!(why.outcome, Outcome::Rejected);
    let first = &why.candidates[0];
    assert_eq!(first.verdict, Verdict::GuardFalse);
    let guard = first.guard.as_ref().unwrap();
    // `all` stops at the part that decides: upstream is not ok yet.
    assert_eq!(guard.result, Some(false));
    assert_eq!(guard.parts.len(), 1);
    assert_eq!(
        serde_json::to_value(&guard.parts[0].guard).unwrap(),
        json!({"in": "#upstream.ok"})
    );
    assert_eq!(why.candidates[1].verdict, Verdict::GuardFalse);

    // Once upstream is ok the same question has a different answer.
    let ready = m.step(&start, &Event::new("upstream.ok")).unwrap().snapshot;
    let why = m.explain(&ready, &event, &quota(true));
    assert_eq!(why.outcome, Outcome::Handled);
    assert_eq!(why.candidates[0].verdict, Verdict::Taken);
    let parts = &why.candidates[0].guard.as_ref().unwrap().parts;
    assert_eq!(parts.len(), 3);
    assert_eq!(
        parts[1].values,
        Some((json!(0), json!(3))),
        "a comparison shows its operands"
    );

    // No quota: the host guard is the reason. Without the host, it cannot be evaluated.
    let why = m.explain(&ready, &event, &quota(false));
    assert_eq!(
        why.candidates[0].guard.as_ref().unwrap().parts[2].result,
        Some(false)
    );
    let why = m.explain(&ready, &event, &NoGuards);
    assert_eq!(why.candidates[0].guard.as_ref().unwrap().result, None);
    assert_eq!(why.candidates[0].verdict, Verdict::GuardFalse);

    // The second transition is enabled by the event's data, and is then the one taken.
    let forced = Event::new("start").with_data(json!({"force": true}));
    let why = m.explain(&ready, &forced, &quota(false));
    assert_eq!(
        why.candidates.iter().map(|c| c.verdict).collect::<Vec<_>>(),
        [Verdict::GuardFalse, Verdict::Taken]
    );
    // With quota both are enabled; the first wins and the second is shadowed.
    let why = m.explain(&ready, &forced, &quota(true));
    assert_eq!(
        why.candidates.iter().map(|c| c.verdict).collect::<Vec<_>>(),
        [Verdict::Taken, Verdict::Shadowed]
    );

    // A transition whose source is not active is listed with that reason.
    let why = m.explain(&ready, &Event::new("progress.epoch"), &NoGuards);
    assert_eq!(why.outcome, Outcome::Rejected);
    assert_eq!(why.candidates[0].verdict, Verdict::SourceInactive);
}

#[test]
fn enabled_lists_what_can_happen_now() {
    let m = machine();
    let start = m.start(Value::Null).unwrap().snapshot;
    let names = |s| -> Vec<(String, bool)> {
        m.enabled(s, &quota(true))
            .into_iter()
            .map(|e| (e.event, e.depends_on_event))
            .collect()
    };
    // `start` is listed because one of its guards reads the event and may hold.
    assert_eq!(
        names(&start),
        [
            ("upstream.ok".to_string(), false),
            ("start".to_string(), true),
            ("skip".to_string(), false)
        ]
    );
    let ready = m.step(&start, &Event::new("upstream.ok")).unwrap().snapshot;
    let running = m
        .step_with(&ready, &Event::new("start"), &quota(true))
        .unwrap()
        .snapshot;
    assert_eq!(
        names(&running),
        [
            ("progress.*".to_string(), false),
            ("after.step.running:after:0".to_string(), false)
        ]
    );
    assert!(m.has_tag(&running, "success"));
    assert_eq!(m.leaves(&running), ["upstream.ok", "step.running"]);
}

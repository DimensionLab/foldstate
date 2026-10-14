// SPDX-License-Identifier: Apache-2.0
use foldstate::{Definition, Event, Guard, MAIN, Machine, NoGuards, TransitionRef, Value};
use foldstate_check::{Bounds, Explorer};
use serde_json::json;

fn machine(v: serde_json::Value) -> Machine {
    let def: Definition = serde_json::from_value(v).expect("parses");
    Machine::compile(&def).expect("compiles")
}

/// Two jobs in sequence: `b` waits for `a`. A cancel is possible while `a` runs.
fn pipeline() -> Machine {
    machine(json!({
        "schema": "foldstate/1", "id": "run",
        "root": {"id": "run", "states": [
            {"id": "active", "kind": "parallel",
             "on": [{"event": "done.state.active", "target": ["finished"]},
                    {"event": "cancel", "target": ["cancelled"]}],
             "states": [
                {"id": "a", "states": [
                    {"id": "running", "on": [{"event": "a.ok", "target": ["ok"]}, {"event": "a.fail", "target": ["failed"]}]},
                    {"id": "failed", "on": [{"event": "a.retry", "target": ["running"]}]},
                    {"id": "ok", "kind": "final"}
                ]},
                {"id": "b", "states": [
                    {"id": "waiting", "on": [{"guard": {"in": "#active.a.ok"}, "target": ["running"]}]},
                    {"id": "running", "on": [{"event": "b.ok", "target": ["ok"]}]},
                    {"id": "ok", "kind": "final"}
                ]}
             ]},
            {"id": "finished", "kind": "final"},
            {"id": "cancelled", "kind": "final"}
        ]}
    }))
}

#[test]
fn a_sound_machine_gives_a_clean_report_with_paths() {
    let m = pipeline();
    let report = Explorer::new(&m)
        .never(
            "b runs before a is ok",
            Guard::All(vec![
                Guard::In("#active.b.running".into()),
                Guard::Not(Box::new(Guard::In("#active.a.ok".into()))),
            ]),
        )
        .run()
        .unwrap();
    assert!(report.is_clean(), "{report:#?}");
    assert!(report.can_finish);
    assert!(report.unreached_states.is_empty() && report.unused_transitions.is_empty());
    // start, a failed, b running (a ok), finished, cancelled
    assert_eq!(report.states, 5);
    let names = |path: &str| -> Vec<&str> {
        report.paths_to_states[path]
            .events
            .iter()
            .map(|e| e.name.as_str())
            .collect()
    };
    assert_eq!(names("active.b.running"), ["a.ok"]);
    assert_eq!(names("finished"), ["a.ok", "b.ok"]);
    assert_eq!(names("active.a.failed"), ["a.fail"]);
    // The retry transition is covered by: fail, retry.
    let retry = &report.paths_to_transitions["active.a.failed[0]"];
    assert_eq!(retry.events.len(), 2);
    // Every path replays.
    for trace in report
        .paths_to_states
        .values()
        .chain(report.paths_to_transitions.values())
    {
        trace.to_log(&m, &NoGuards).unwrap().verify(&m).unwrap();
    }
}

#[test]
fn findings_come_with_the_shortest_trace() {
    // `b` can be skipped, after which nothing completes the parallel state; `stuck` has no
    // way out; `orphan` is never entered; one action overflows.
    let m = machine(json!({
        "schema": "foldstate/1", "id": "run", "context": {"n": 9223372036854775807i64},
        "root": {"id": "run", "states": [
            {"id": "work", "on": [
                {"event": "go", "target": ["stuck"]},
                {"event": "finish", "target": ["end"]},
                {"event": "count", "actions": [{"assign": {"path": "/n", "value": {"add": [{"ctx": "/n"}, {"lit": 1}]}}}]},
                {"event": "never", "guard": {"in": "#orphan"}, "target": ["end"]}
            ]},
            {"id": "stuck"},
            {"id": "orphan"},
            {"id": "end", "kind": "final"}
        ]}
    }));
    let report = Explorer::new(&m)
        .always(
            "never stuck",
            Guard::Not(Box::new(Guard::In("#stuck".into()))),
        )
        .always_fn("context has n", |_, s| s.context().get("n").is_some())
        .run()
        .unwrap();
    assert!(report.complete && !report.is_clean());
    assert_eq!(report.unreached_states, ["orphan"]);
    // `count` always fails, so it is never taken either.
    assert_eq!(
        report.unused_transitions,
        [
            TransitionRef {
                source: "work".into(),
                index: 2
            },
            TransitionRef {
                source: "work".into(),
                index: 3
            }
        ]
    );
    assert_eq!(report.deadlocks.len(), 1);
    assert_eq!(report.deadlocks[0].events, [Event::new("go")]);
    assert_eq!(report.violations.len(), 1);
    assert_eq!(report.violations[0].invariant, "never stuck");
    assert_eq!(report.failures.len(), 1);
    assert!(
        report.failures[0].error.contains("overflow"),
        "{}",
        report.failures[0].error
    );
    assert_eq!(report.failures[0].trace.events, [Event::new("count")]);
    assert!(report.can_finish);
    assert_eq!(
        report.cannot_finish_from.as_ref().unwrap().events,
        [Event::new("go")]
    );

    // The counterexample is a log: its head is the offending state.
    let log = report.deadlocks[0].to_log(&m, &NoGuards).unwrap();
    assert!(m.is_in(log.head(MAIN).unwrap(), "stuck"));
}

#[test]
fn timers_are_part_of_the_alphabet_and_bounds_are_reported() {
    let m = machine(json!({
        "schema": "foldstate/1", "id": "lease", "context": {"n": 0},
        "root": {"id": "lease", "states": [
            {"id": "held", "after": [{"delay_ms": 1000, "target": ["lost"]}],
             "on": [{"event": "beat", "target": ["held"], "actions": [
                 {"assign": {"path": "/n", "value": {"add": [{"ctx": "/n"}, {"lit": 1}]}}}
             ]}]},
            {"id": "lost", "kind": "final"}
        ]}
    }));
    // The counter makes the state space unbounded.
    let report = Explorer::new(&m)
        .bounds(Bounds {
            max_states: 50,
            max_depth: usize::MAX,
        })
        .run()
        .unwrap();
    assert!(!report.complete && !report.is_clean());
    assert_eq!(report.states, 50);
    // The timer's event was tried without being listed: `lost` is reached.
    assert_eq!(
        report.paths_to_states["lost"].events[0].name,
        "after.held:after:0"
    );
    assert!(report.deadlocks.is_empty());

    // Data-dependent guards need events with data in the alphabet.
    let m = machine(json!({
        "schema": "foldstate/1", "id": "gate",
        "root": {"id": "gate", "states": [
            {"id": "shut", "on": [{"event": "knock", "target": ["open"],
                "guard": {"cmp": {"left": {"event": "/code"}, "op": "eq", "right": {"lit": 42}}}}]},
            {"id": "open", "kind": "final"}
        ]}
    }));
    let blind = Explorer::new(&m).run().unwrap();
    assert_eq!(blind.unreached_states, ["open"]);
    let informed = Explorer::new(&m)
        .event(Event::new("knock").with_data(json!({"code": 42})))
        .input(Value::Null)
        .run()
        .unwrap();
    assert!(informed.is_clean());
}

/// The stateright adapter sees the same state space as the explorer.
#[cfg(feature = "stateright")]
#[test]
fn stateright_finds_the_same_states() {
    use foldstate_check::stateright::Chart;
    use stateright::{Checker, Model};

    let m = pipeline();
    let report = Explorer::new(&m).run().unwrap();
    let checker = Chart::new(m)
        .sometimes("finishes", |chart, s| {
            chart.machine().is_in(s.snapshot(), "finished")
        })
        .always("b waits for a", |chart, s| {
            !chart.machine().is_in(s.snapshot(), "active.b.running")
                || chart.machine().is_in(s.snapshot(), "active.a.ok")
        })
        .checker()
        .spawn_bfs()
        .join();
    checker.assert_properties();
    assert_eq!(checker.unique_state_count(), report.states);
}

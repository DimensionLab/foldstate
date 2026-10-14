// SPDX-License-Identifier: Apache-2.0
//! The reference model against hand-worked cases. Each case states a rule of
//! `docs/semantics.md` and the result it must give; the kernel is then tested against the
//! reference model on generated definitions (`tests/differential.rs`).

mod common;

use common::generate::{self, Rng};
use common::reference::{Interpreter, Model, RefError};
use foldstate::{Definition, Effect, Event, Status, Value, WireSnapshot};
use serde_json::json;

fn def(v: serde_json::Value) -> Definition {
    serde_json::from_value(v).expect("definition parses")
}

struct Run<'a> {
    interp: Interpreter<'a, 'static>,
    snapshot: WireSnapshot,
    effects: Vec<Effect>,
}

impl<'a> Run<'a> {
    fn start(model: &'a Model<'a>) -> Self {
        let interp = Interpreter::new(model, "", &common::guards);
        let (snapshot, effects) = interp.start(&Value::Null).expect("starts");
        Run {
            interp,
            snapshot,
            effects,
        }
    }

    fn send(&mut self, name: &str) -> Result<bool, RefError> {
        self.send_event(Event::new(name))
    }

    fn send_event(&mut self, event: Event) -> Result<bool, RefError> {
        let (snapshot, effects, handled) = self.interp.step(&self.snapshot, &event)?;
        self.snapshot = snapshot;
        self.effects = effects;
        Ok(handled)
    }

    fn config(&self) -> Vec<&str> {
        self.snapshot
            .configuration
            .iter()
            .map(String::as_str)
            .collect()
    }

    fn emitted(&self) -> Vec<&str> {
        self.effects
            .iter()
            .filter_map(|e| match e {
                Effect::Emit { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect()
    }
}

fn emit(name: &str) -> serde_json::Value {
    json!({"emit": {"name": name}})
}

#[test]
fn flat_machine_rejects_and_ignores() {
    let d = def(json!({
        "schema": "foldstate/1", "id": "m", "ignore": ["noise.*"],
        "root": {"id": "m", "states": [
            {"id": "a", "on": [{"event": "go", "target": ["b"]}]},
            {"id": "b"}
        ]}
    }));
    let m = Model::new(&d);
    let mut r = Run::start(&m);
    assert_eq!(r.config(), ["a"]);
    assert_eq!(r.send("stop"), Err(RefError::Rejected));
    assert_eq!(r.send("noise.loud"), Ok(false));
    assert_eq!(r.send("noise"), Ok(false));
    assert_eq!(r.send("noisy"), Err(RefError::Rejected));
    assert_eq!(r.send("go"), Ok(true));
    assert_eq!(r.config(), ["b"]);
    assert_eq!(r.send("go"), Err(RefError::Rejected));
}

#[test]
fn descendants_take_priority_and_order_within_a_state_matters() {
    let d = def(json!({
        "schema": "foldstate/1", "id": "m",
        "root": {"id": "m", "states": [
            {"id": "p", "on": [{"event": "e", "target": ["x"]}], "states": [
                {"id": "c", "on": [
                    {"event": "e", "guard": {"cmp": {"left": {"lit": 1}, "op": "eq", "right": {"lit": 2}}}, "target": ["#x"]},
                    {"event": "e", "target": ["d"]},
                    {"event": "e", "target": ["#x"]}
                ]},
                {"id": "d"}
            ]},
            {"id": "x"}
        ]}
    }));
    let m = Model::new(&d);
    let mut r = Run::start(&m);
    assert_eq!(r.config(), ["p", "p.c"]);
    r.send("e").unwrap();
    assert_eq!(
        r.config(),
        ["p", "p.d"],
        "the child's first enabled transition wins"
    );
    r.send("e").unwrap();
    assert_eq!(r.config(), ["x"], "then the parent's");
}

#[test]
fn exit_actions_run_innermost_first_then_transition_then_entry_outermost_first() {
    let d = def(json!({
        "schema": "foldstate/1", "id": "m",
        "root": {"id": "m", "states": [
            {"id": "a", "exit": [emit("exit a")], "states": [
                {"id": "a1", "exit": [emit("exit a1")], "on": [
                    {"event": "go", "target": ["#b.b1"], "actions": [emit("transition")]}
                ]}
            ]},
            {"id": "b", "entry": [emit("enter b")], "states": [
                {"id": "b0"},
                {"id": "b1", "entry": [emit("enter b1")]}
            ]}
        ]}
    }));
    let m = Model::new(&d);
    let mut r = Run::start(&m);
    r.send("go").unwrap();
    assert_eq!(
        r.emitted(),
        ["exit a1", "exit a", "transition", "enter b", "enter b1"]
    );
    assert_eq!(r.config(), ["b", "b.b1"]);
}

#[test]
fn internal_transition_keeps_its_source_external_re_enters_it() {
    let d = def(json!({
        "schema": "foldstate/1", "id": "m",
        "root": {"id": "m", "states": [
            {"id": "p", "entry": [emit("enter p")], "exit": [emit("exit p")],
             "on": [
                {"event": "in", "target": [".y"], "internal": true},
                {"event": "out", "target": [".y"]},
                {"event": "note", "actions": [emit("noted")]}
             ],
             "states": [{"id": "x"}, {"id": "y"}]}
        ]}
    }));
    let m = Model::new(&d);
    let mut r = Run::start(&m);
    r.send("in").unwrap();
    assert_eq!(r.emitted(), Vec::<&str>::new());
    assert_eq!(r.config(), ["p", "p.y"]);
    r.send("out").unwrap();
    assert_eq!(r.emitted(), ["exit p", "enter p"]);
    r.send("note").unwrap();
    assert_eq!(r.emitted(), ["noted"], "no target: actions only");
    assert_eq!(r.config(), ["p", "p.y"]);
}

fn two_regions() -> Definition {
    def(json!({
        "schema": "foldstate/1", "id": "m",
        "root": {"id": "m", "states": [
            {"id": "work", "kind": "parallel",
             "on": [{"event": "done.state.work", "target": ["finished"]}],
             "states": [
                {"id": "left", "states": [
                    {"id": "l1", "on": [{"event": "e", "target": ["l2"]}, {"event": "leave", "target": ["#aborted"]}]},
                    {"id": "l2", "kind": "final"}
                ]},
                {"id": "right", "states": [
                    {"id": "r1", "on": [{"event": "e", "target": ["r2"]}, {"event": "leave", "target": ["r2"]}, {"event": "f", "target": ["r2"]}]},
                    {"id": "r2", "kind": "final"}
                ]}
             ]},
            {"id": "aborted"},
            {"id": "finished", "kind": "final"}
        ]}
    }))
}

#[test]
fn one_event_moves_every_region_and_done_events_cascade() {
    let d = two_regions();
    let m = Model::new(&d);
    let mut r = Run::start(&m);
    assert_eq!(
        r.config(),
        [
            "work",
            "work.left",
            "work.left.l1",
            "work.right",
            "work.right.r1"
        ]
    );
    r.send("e").unwrap();
    // Both regions reach their final state in one microstep; that completes `work`, whose
    // done event moves the machine to its own final state within the same macrostep.
    assert_eq!(r.config(), ["finished"]);
    assert_eq!(r.snapshot.status, Status::Done);
    assert_eq!(r.send("e"), Err(RefError::Done));
}

#[test]
fn a_transition_that_leaves_the_parallel_state_preempts_later_regions() {
    let d = two_regions();
    let m = Model::new(&d);
    let mut r = Run::start(&m);
    r.send("leave").unwrap();
    assert_eq!(
        r.config(),
        ["aborted"],
        "left's transition exits `work`; right's conflicting transition is dropped"
    );
}

#[test]
fn a_region_alone_does_not_complete_the_parallel_state() {
    let d = two_regions();
    let m = Model::new(&d);
    let mut r = Run::start(&m);
    r.send("f").unwrap();
    assert_eq!(
        r.config(),
        [
            "work",
            "work.left",
            "work.left.l1",
            "work.right",
            "work.right.r2"
        ]
    );
    assert_eq!(r.snapshot.status, Status::Active);
}

#[test]
fn shallow_and_deep_history() {
    let d = def(json!({
        "schema": "foldstate/1", "id": "m",
        "root": {"id": "m", "states": [
            {"id": "on", "states": [
                {"id": "h", "kind": "history"},
                {"id": "hd", "kind": "history", "deep": true, "default": ["b"]},
                {"id": "a", "on": [{"event": "next", "target": ["b.b2"]}], "states": [{"id": "a1"}]},
                {"id": "b", "states": [{"id": "b1"}, {"id": "b2"}]}
            ], "on": [{"event": "off", "target": ["off"]}]},
            {"id": "off", "on": [
                {"event": "shallow", "target": ["#on.h"]},
                {"event": "deep", "target": ["#on.hd"]}
            ]}
        ]}
    }));
    let m = Model::new(&d);
    let mut r = Run::start(&m);
    r.send("off").unwrap();
    r.send("deep").unwrap();
    assert_eq!(
        r.config(),
        ["on", "on.a", "on.a.a1"],
        "deep history restores what was active"
    );
    r.send("next").unwrap();
    assert_eq!(r.config(), ["on", "on.b", "on.b.b2"]);
    r.send("off").unwrap();
    r.send("shallow").unwrap();
    assert_eq!(
        r.config(),
        ["on", "on.b", "on.b.b1"],
        "shallow history restores the child, entered by its default"
    );
    r.send("off").unwrap();
    r.send("deep").unwrap();
    assert_eq!(r.config(), ["on", "on.b", "on.b.b1"]);

    // Without any history, the default applies.
    let mut fresh = Run::start(&m);
    fresh.snapshot.configuration = vec!["off".into()];
    fresh.send("deep").unwrap();
    assert_eq!(fresh.config(), ["on", "on.b", "on.b.b1"]);
}

#[test]
fn raised_events_are_processed_before_the_macrostep_ends_and_eventless_first() {
    let d = def(json!({
        "schema": "foldstate/1", "id": "m", "context": {"n": 0},
        "root": {"id": "m", "states": [
            {"id": "a", "on": [{"event": "go", "target": ["b"], "actions": [
                {"raise": {"event": "second"}}, {"raise": {"event": "third"}}, emit("go")
            ]}]},
            {"id": "b", "on": [
                {"target": ["c"], "actions": [emit("eventless")]},
                {"event": "second", "target": ["d"]}
            ]},
            {"id": "c", "on": [
                {"event": "second", "actions": [emit("second in c")]},
                {"event": "third", "target": ["d"], "actions": [emit("third")]}
            ]},
            {"id": "d", "on": [{"guard": {"cmp": {"left": {"ctx": "/n"}, "op": "lt", "right": {"lit": 3}}},
                                 "actions": [{"assign": {"path": "/n", "value": {"add": [{"ctx": "/n"}, {"lit": 1}]}}}]}]}
        ]}
    }));
    let m = Model::new(&d);
    let mut r = Run::start(&m);
    r.send("go").unwrap();
    assert_eq!(r.emitted(), ["go", "eventless", "second in c", "third"]);
    assert_eq!(r.config(), ["d"]);
    assert_eq!(
        r.snapshot.context,
        json!({"n": 3}),
        "the eventless loop ran until its guard failed"
    );
}

#[test]
fn timers_and_invocations_follow_their_state() {
    let d = def(json!({
        "schema": "foldstate/1", "id": "m",
        "root": {"id": "m", "states": [
            {"id": "idle", "on": [{"event": "start", "target": ["running"]}]},
            {"id": "running",
             "invoke": [{"name": "job", "params": {"lit": {"image": "x"}}}],
             "after": [{"delay_ms": 5000, "target": ["failed"]}],
             "on": [{"event": "ok", "target": ["idle"]}, {"event": "bounce", "target": ["idle"], "actions": [{"raise": {"event": "start"}}]}]},
            {"id": "failed", "kind": "final"}
        ]}
    }));
    let m = Model::new(&d);
    let mut r = Run::start(&m);
    r.send_event(Event::new("start").at(1000)).unwrap();
    assert_eq!(
        r.effects,
        [
            Effect::Schedule {
                id: "running:after:0".into(),
                event: "after.running:after:0".into(),
                delay_ms: 5000,
                due_ms: Some(6000)
            },
            Effect::Start {
                id: "running:invoke:0".into(),
                name: "job".into(),
                params: json!({"image": "x"})
            },
        ]
    );
    assert!(r.snapshot.timers.contains_key("running:after:0"));
    assert!(r.snapshot.invocations.contains_key("running:invoke:0"));

    r.send("ok").unwrap();
    assert_eq!(
        r.effects,
        [
            Effect::Cancel {
                id: "running:after:0".into()
            },
            Effect::Stop {
                id: "running:invoke:0".into()
            },
        ]
    );
    assert!(r.snapshot.timers.is_empty() && r.snapshot.invocations.is_empty());

    r.send("start").unwrap();
    // Leaving and coming back within one macrostep: the old work is stopped, the new started.
    r.send("bounce").unwrap();
    assert_eq!(r.config(), ["running"]);
    assert!(matches!(r.effects.first(), Some(Effect::Cancel { .. })));
    assert!(matches!(r.effects.get(1), Some(Effect::Stop { .. })));
    assert!(matches!(r.effects.last(), Some(Effect::Start { .. })));

    // The timer fires: it is no longer pending, the machine finishes and stops the job.
    r.send("after.running:after:0").unwrap();
    assert_eq!(r.snapshot.status, Status::Done);
    assert_eq!(
        r.effects,
        [Effect::Stop {
            id: "running:invoke:0".into()
        }]
    );
    assert!(r.snapshot.timers.is_empty() && r.snapshot.invocations.is_empty());
}

#[test]
fn livelock_and_failing_actions_change_nothing() {
    let d = def(json!({
        "schema": "foldstate/1", "id": "m", "context": {"n": 0}, "max_microsteps": 10,
        "root": {"id": "m", "states": [
            {"id": "a", "on": [
                {"event": "spin", "target": ["b"]},
                {"event": "bad", "target": ["c"], "actions": [{"assign": {"path": "/n/x", "value": {"lit": 1}}}]}
            ]},
            {"id": "b", "on": [{"target": ["b"]}]},
            {"id": "c"}
        ]}
    }));
    let m = Model::new(&d);
    let mut r = Run::start(&m);
    let before = r.snapshot.clone();
    assert_eq!(r.send("spin"), Err(RefError::Livelock));
    assert_eq!(r.send("bad"), Err(RefError::Eval));
    assert_eq!(r.snapshot, before);
}

#[test]
fn guards_that_cannot_be_evaluated_are_false() {
    let d = def(json!({
        "schema": "foldstate/1", "id": "m", "context": {"n": 1},
        "root": {"id": "m", "states": [
            {"id": "a", "on": [
                {"event": "e", "guard": {"named": {"name": "missing"}}, "target": ["b"]},
                {"event": "e", "guard": {"cmp": {"left": {"add": [{"ctx": "/nope"}, {"lit": 1}]}, "op": "gt", "right": {"lit": 0}}}, "target": ["b"]},
                {"event": "e", "guard": {"named": {"name": "odd"}}, "target": ["c"]}
            ]},
            {"id": "b"}, {"id": "c"}
        ]}
    }));
    let m = Model::new(&d);
    let mut r = Run::start(&m);
    r.send("e").unwrap();
    assert_eq!(r.config(), ["c"]);
}

#[test]
fn root_transitions_and_parallel_root() {
    let d = def(json!({
        "schema": "foldstate/1", "id": "m",
        "root": {"id": "m", "kind": "parallel",
            "on": [{"event": "reset", "target": [".x.x2"]}],
            "states": [
                {"id": "x", "states": [{"id": "x1", "on": [{"event": "e", "target": ["x2"]}]}, {"id": "x2"}, {"id": "xf", "kind": "final"}],
                 "on": [{"event": "fin", "target": [".xf"], "internal": true}]},
                {"id": "y", "states": [{"id": "y1", "on": [{"event": "e", "target": ["y2"]}]}, {"id": "y2"}, {"id": "yf", "kind": "final"}],
                 "on": [{"event": "fin", "target": [".yf"], "internal": true}]}
            ]}
    }));
    let m = Model::new(&d);
    let mut r = Run::start(&m);
    r.send("e").unwrap();
    assert_eq!(r.config(), ["x", "x.x2", "y", "y.y2"]);
    r.send("reset").unwrap();
    assert_eq!(
        r.config(),
        ["x", "x.x2", "y", "y.y1"],
        "a root transition re-enters everything below the root; other regions by default"
    );
    // Internal, or each region's transition would have the root as its domain and exit the
    // other region.
    r.send("fin").unwrap();
    assert_eq!(r.config(), ["x", "x.xf", "y", "y.yf"]);
    assert_eq!(
        r.snapshot.status,
        Status::Done,
        "all regions of a parallel root are final"
    );
}

#[test]
fn generated_definitions_run_without_panicking() {
    let mut rng = Rng::new(7);
    let (mut handled, mut refused) = (0u32, 0u32);
    for _ in 0..300 {
        let d = generate::definition(&mut rng);
        let m = Model::new(&d);
        let interp = Interpreter::new(&m, "", &common::guards);
        let Ok((mut snapshot, _)) = interp.start(&Value::Null) else {
            continue;
        };
        for step in 0..30 {
            let event = generate::event(&mut rng, &snapshot, step);
            match interp.step(&snapshot, &event) {
                Ok((next, _, _)) => {
                    snapshot = next;
                    handled += 1;
                }
                Err(_) => refused += 1,
            }
        }
    }
    // The generator must exercise both outcomes, or the differential test proves little.
    assert!(
        handled > 1000 && refused > 300,
        "handled {handled}, refused {refused}"
    );
}

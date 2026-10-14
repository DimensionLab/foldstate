// SPDX-License-Identifier: Apache-2.0
//! foldstate against XState v5 on generated statecharts.
//!
//! Opt-in, because it needs Node and the pinned `xstate` package:
//!
//! ```text
//! (cd crates/foldstate/tests/xstate && npm install)
//! FOLDSTATE_XSTATE=1 cargo test -p foldstate --test xstate -- --nocapture
//! ```
//!
//! The generated definitions are restricted to what both libraries express the same way:
//! compound, parallel, final and history states; transitions on exact event names, with
//! one or several targets, internal or external; eventless transitions; guards over active
//! states and a counter; a counter assignment and raised events. After every event the
//! active states, the counter and whether the machine is done are compared.

mod common;

use std::process::Command;

use common::generate::{self, Rng};
use foldstate::{
    Action, Definition, Event, Expr, Guard, Kind, Machine, StateDef, StepError, Value,
};
use serde_json::json;

fn keep_guard(g: Guard) -> Option<Guard> {
    match g {
        Guard::In(_) => Some(g),
        Guard::Not(inner) => keep_guard(*inner).map(|g| Guard::Not(Box::new(g))),
        Guard::All(gs) => Some(Guard::All(gs.into_iter().filter_map(keep_guard).collect())),
        Guard::Any(gs) => {
            let kept: Vec<Guard> = gs.into_iter().filter_map(keep_guard).collect();
            (!kept.is_empty()).then_some(Guard::Any(kept))
        }
        Guard::Cmp {
            left: Expr::Ctx(p),
            op,
            right: Expr::Lit(k),
        } if p == "/n" => Some(Guard::Cmp {
            left: Expr::Ctx(p),
            op,
            right: Expr::Lit(k),
        }),
        _ => None,
    }
}

fn keep_actions(actions: Vec<Action>) -> Vec<Action> {
    actions
        .into_iter()
        .filter(|a| match a {
            Action::Assign { path, value } => path == "/n" && matches!(value, Expr::Add(_, _)),
            Action::Raise { .. } | Action::Emit { .. } => true,
            _ => false,
        })
        .map(|a| match a {
            Action::Raise { event, .. } => Action::Raise { event, data: None },
            Action::Emit { name, .. } => Action::Emit { name, params: None },
            other => other,
        })
        .collect()
}

fn kind_at(tree: &StateDef, path: &[&str]) -> Kind {
    path.iter()
        .fold(tree, |s, id| {
            s.states.iter().find(|c| c.id == *id).expect("path exists")
        })
        .resolved_kind()
}

/// XState takes the nearest common ancestor of a transition's source and targets as its
/// domain; SCXML and foldstate take the nearest one that is a compound state, so that a
/// transition never leaves a parallel state half exited. The two agree unless that nearest
/// ancestor is a parallel state below the root.
fn domain_differs(tree: &StateDef, source: &str, t: &foldstate::TransitionDef) -> bool {
    if t.target.is_empty() {
        return false;
    }
    let source: Vec<&str> = source.split('.').collect();
    let targets: Vec<Vec<&str>> = t
        .target
        .iter()
        .map(|r| r[1..].split('.').collect())
        .collect();
    let inside = targets
        .iter()
        .all(|p| p.len() > source.len() && p[..source.len()] == source[..]);
    if t.internal && kind_at(tree, &source) == Kind::Compound && inside {
        return false;
    }
    // The longest proper ancestor shared by the source and every target.
    let mut common = source.len() - 1;
    for p in &targets {
        let shared = source.iter().zip(p).take_while(|(a, b)| a == b).count();
        common = common.min(shared).min(p.len() - 1);
    }
    common > 0 && kind_at(tree, &source[..common]) == Kind::Parallel
}

/// Restricts a generated definition to the common subset.
fn restrict(s: &mut StateDef, root: bool) {
    s.after.clear();
    s.invoke.clear();
    s.entry = keep_actions(std::mem::take(&mut s.entry));
    s.exit = keep_actions(std::mem::take(&mut s.exit));
    let mut on = Vec::new();
    for mut t in std::mem::take(&mut s.on) {
        if root {
            continue; // XState's root transitions re-enter the root; ours do not.
        }
        if let Some(e) = &t.event
            && (e.contains('*') || e.contains(' ') || e.starts_with("done."))
        {
            continue;
        }
        let had_guard = t.guard.is_some();
        t.guard = t.guard.take().and_then(keep_guard);
        if t.event.is_none() && (!had_guard || t.guard.is_none()) {
            continue; // an eventless transition must stay bounded by its guard
        }
        t.actions = keep_actions(std::mem::take(&mut t.actions));
        on.push(t);
    }
    s.on = on;
    for c in &mut s.states {
        restrict(c, false);
    }
}

fn drop_differing(s: &mut StateDef, path: &str, root: bool, tree: &StateDef) {
    if !root {
        s.on.retain(|t| !domain_differs(tree, path, t));
    }
    for c in &mut s.states {
        let p = if root {
            c.id.clone()
        } else {
            format!("{path}.{}", c.id)
        };
        drop_differing(c, &p, false, tree);
    }
}

/// The events handled somewhere below `s`, in a region of a parallel state (`None`: an
/// eventless transition).
fn handled_in_regions(s: &StateDef, through_parallel: bool, out: &mut Vec<Option<String>>) {
    let through = through_parallel || s.resolved_kind() == Kind::Parallel;
    for c in &s.states {
        if through {
            out.extend(c.on.iter().map(|t| t.event.clone()));
        }
        handled_in_regions(c, through, out);
    }
}

/// In SCXML and foldstate every active atomic state looks for a transition on its own, so a
/// transition of an ancestor is taken for the regions that have none of their own. XState
/// does not take the ancestor's transition once any region handles the event. Those
/// ancestor transitions are left out.
fn drop_shadowed(s: &mut StateDef) {
    let mut below = Vec::new();
    handled_in_regions(s, false, &mut below);
    // Eventless transitions count as one more event.
    s.on.retain(|t| !below.contains(&t.event));
    for c in &mut s.states {
        drop_shadowed(c);
    }
}

fn has_history(s: &StateDef) -> bool {
    s.kind == Some(Kind::History) || s.states.iter().any(has_history)
}

/// The generated cases that still differ from XState, with what XState gives at start
/// (recorded from xstate 5.33.2): seed, active states, counter.
const ENTRY_ORDER: [(u64, &[&str], i64); 2] = [
    (1335, &["s1", "s1.s2", "s4", "s4.s5", "s4.s6"], 8),
    (15567, &["s1", "s1.s3", "s4", "s4.s6"], 5),
];

/// The definition the oracle test builds for a seed.
fn restricted(seed: u64) -> (Definition, Rng) {
    let mut rng = Rng::new(seed);
    let mut def: Definition = generate::definition(&mut rng);
    restrict(&mut def.root, true);
    let tree = def.root.clone();
    drop_differing(&mut def.root, "", true, &tree);
    drop_shadowed(&mut def.root);
    def.unhandled = foldstate::Unhandled::Ignore;
    def.ignore.clear();
    def.max_microsteps = 200;
    (def, rng)
}

/// When two enabled transitions conflict, SCXML and foldstate keep the one that belongs to
/// the atomic state that comes first in the document. XState keeps the one whose atomic
/// state was entered first. The reference model, told to use XState's order, reproduces
/// XState's result for both remaining cases; in document order it gives foldstate's.
/// Runs without Node.
#[test]
fn the_remaining_differences_are_the_order_of_conflicting_transitions() {
    use common::reference::{Interpreter, Model};
    for (seed, xstate_config, xstate_n) in ENTRY_ORDER {
        let (def, _) = restricted(seed);
        let machine = Machine::compile(&def).unwrap();
        let model = Model::new(&def);
        let observe = |w: &foldstate::WireSnapshot| {
            let mut config = w.configuration.clone();
            config.sort();
            (config, w.context["n"].as_i64().unwrap())
        };
        let ours = machine.to_wire(&machine.start(Value::Null).unwrap().snapshot);
        let document_order = Interpreter::new(&model, machine.hash(), &common::guards)
            .start(&Value::Null)
            .unwrap()
            .0;
        let entry_order = Interpreter::new(&model, machine.hash(), &common::guards)
            .in_entry_order()
            .start(&Value::Null)
            .unwrap()
            .0;
        assert_eq!(observe(&ours), observe(&document_order), "seed {seed}");
        let want: Vec<String> = xstate_config.iter().map(|s| s.to_string()).collect();
        assert_eq!(observe(&entry_order), (want, xstate_n), "seed {seed}");
        assert_ne!(observe(&ours), observe(&entry_order), "seed {seed}");
    }
}

/// The smallest machine that shows it: after `a1 -> a2`, the transitions of `a2` and `b1`
/// are both enabled and conflict. `a2` comes first in the document; `b1` was entered first.
#[test]
fn conflicting_transitions_are_taken_in_document_order() {
    let def: Definition = serde_json::from_value(json!({
        "schema": "foldstate/1", "id": "order", "context": {"n": 0},
        "root": {"id": "order", "kind": "parallel", "states": [
            {"id": "a", "states": [
                {"id": "a1", "on": [{"guard": {"cmp": {"left": {"ctx": "/n"}, "op": "eq", "right": {"lit": 0}}},
                    "target": ["#a.a2"], "actions": [{"assign": {"path": "/n", "value": {"add": [{"ctx": "/n"}, {"lit": 1}]}}}]}]},
                {"id": "a2", "on": [{"guard": {"cmp": {"left": {"ctx": "/n"}, "op": "eq", "right": {"lit": 1}}},
                    "target": ["#b.b2"], "actions": [{"assign": {"path": "/n", "value": {"add": [{"ctx": "/n"}, {"lit": 1}]}}}]}]}
            ]},
            {"id": "b", "states": [
                {"id": "b1", "on": [{"guard": {"cmp": {"left": {"ctx": "/n"}, "op": "eq", "right": {"lit": 1}}},
                    "target": ["#a.a1"], "actions": [
                        {"assign": {"path": "/n", "value": {"add": [{"ctx": "/n"}, {"lit": 1}]}}},
                        {"assign": {"path": "/n", "value": {"add": [{"ctx": "/n"}, {"lit": 1}]}}}]}]},
                {"id": "b2"}
            ]}
        ]}
    }))
    .unwrap();
    let machine = Machine::compile(&def).unwrap();
    let start = machine.start(Value::Null).unwrap().snapshot;
    // foldstate and SCXML: a2's transition. XState 5.33.2 gives ["a.a1", "b.b1"] and n = 3.
    assert_eq!(machine.leaves(&start), ["a.a1", "b.b2"]);
    assert_eq!(start.context()["n"], 2);
}

#[test]
fn agrees_with_xstate_on_the_common_subset() {
    if std::env::var("FOLDSTATE_XSTATE").is_err() {
        println!("skipped: set FOLDSTATE_XSTATE=1 (needs Node and `npm install` in tests/xstate)");
        return;
    }
    let cases: u64 = std::env::var("FOLDSTATE_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000);
    let with_history = std::env::var("FOLDSTATE_XSTATE_HISTORY").is_ok();
    let mut out = Vec::new();
    let mut seed = 0u64;
    while (out.len() as u64) < cases {
        seed += 1;
        let (def, mut rng) = restricted(seed);
        if !with_history && has_history(&def.root) {
            continue;
        }
        let Ok(machine) = Machine::compile(&def) else {
            continue;
        };
        let Ok(start) = machine.start(Value::Null) else {
            continue;
        };
        let observe = |s: &foldstate::Snapshot| {
            let mut config: Vec<&str> = machine.configuration(s);
            config.sort();
            json!({"config": config, "n": s.context()["n"], "done": s.is_done()})
        };
        let mut snapshot = start.snapshot;
        let first = observe(&snapshot);
        let (mut events, mut expected) = (Vec::new(), Vec::new());
        for _ in 0..30 {
            let name = *rng.pick(&["a", "b", "c", "d", "a.x", "i"]);
            match machine.step(&snapshot, &Event::new(name)) {
                Ok(step) => snapshot = step.snapshot,
                Err(StepError::Done { .. }) => break,
                // A livelock or a failing action: stop the case here.
                Err(_) => break,
            }
            events.push(name);
            expected.push(observe(&snapshot));
        }
        out.push(json!({"seed": seed, "definition": def, "start": first, "events": events, "expected": expected}));
    }
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/xstate");
    let file = std::env::temp_dir().join(format!("foldstate-xstate-{}.json", std::process::id()));
    std::fs::write(&file, serde_json::to_vec(&json!({"cases": out})).unwrap()).unwrap();
    let output = Command::new("node")
        .arg("oracle.mjs")
        .arg(&file)
        .current_dir(&dir)
        .output()
        .expect("node runs");
    if std::env::var("FOLDSTATE_XSTATE_KEEP").is_ok() {
        println!("cases kept in {}", file.display());
    } else {
        let _ = std::fs::remove_file(&file);
    }
    if !output.status.success() {
        println!("node: {}", String::from_utf8_lossy(&output.stderr));
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    let summary: serde_json::Value = lines
        .last()
        .and_then(|l| serde_json::from_str(l).ok())
        .unwrap_or_else(|| {
            panic!(
                "no summary from node: {stdout}\n{}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
    println!("xstate: {summary}");
    // See `the_remaining_differences_are_the_order_of_conflicting_transitions`.
    let unexpected: Vec<&&str> = lines[..lines.len() - 1]
        .iter()
        .filter(|l| {
            serde_json::from_str::<serde_json::Value>(l)
                .ok()
                .and_then(|v| v["seed"].as_u64())
                .is_none_or(|seed| !ENTRY_ORDER.iter().any(|(s, _, _)| *s == seed))
        })
        .collect();
    if with_history {
        // History is compared for information only: the two differ by design when a history
        // state of an ancestor is targeted from inside it (docs/semantics.md).
        println!("with history states: {} cases differ", unexpected.len());
        return;
    }
    assert!(
        unexpected.is_empty(),
        "{} of {} cases differ from XState: {:?}",
        unexpected.len(),
        summary["cases"],
        unexpected.iter().take(3).collect::<Vec<_>>()
    );
}

// SPDX-License-Identifier: Apache-2.0
//! The kernel against the reference model, on generated definitions and event sequences.
//!
//! After every event the two must agree on the outcome, the snapshot and the effects. The
//! same runs check the kernel's own invariants: the configuration is legal, a refused event
//! changes nothing, a change can be redone and undone, and snapshots survive serialisation.
//!
//! `FOLDSTATE_CASES` sets the number of definitions (default 2000); `FOLDSTATE_SEED` the
//! first seed. A failure prints the seed, the definition and the events that reproduce it.

mod common;

use common::generate::{self, Rng};
use common::reference::{Interpreter, Model, RefError};
use foldstate::{Definition, Event, Machine, Outcome, StepError, Value, Verdict, WireSnapshot};

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn class(e: &StepError) -> RefError {
    match e {
        StepError::Rejected { .. } => RefError::Rejected,
        StepError::Done { .. } => RefError::Done,
        StepError::Livelock { .. } => RefError::Livelock,
        StepError::Eval { .. } => RefError::Eval,
        StepError::MachineMismatch => panic!("machine mismatch"),
    }
}

#[derive(Default)]
struct Tally {
    definitions: u64,
    events: u64,
    handled: u64,
    ignored: u64,
    rejected: u64,
    done: u64,
    livelock: u64,
    eval: u64,
    start_failed: u64,
    multi: u64,
}

/// Runs one generated case. On a disagreement, the definition and the events are reduced
/// to something small that still disagrees, and that is what the error describes.
fn case(seed: u64, steps: usize, tally: &mut Tally) -> Result<(), String> {
    let mut rng = Rng::new(seed);
    let def = generate::definition(&mut rng);
    let mut events = Vec::new();
    let Err(what) = drive(&def, Some(&mut rng), steps, &mut events, tally) else {
        return Ok(());
    };
    let (small, few, small_what) = minimise(def.clone(), events.clone(), what.clone());
    Err(format!(
        "seed {seed}: {what}\n\nreduced to:\n{small_what}\ndefinition:\n{}\nevents:\n{}\n\noriginal definition:\n{}\noriginal events:\n{}",
        serde_json::to_string_pretty(&small).unwrap(),
        serde_json::to_string(&few).unwrap(),
        serde_json::to_string(&def).unwrap(),
        serde_json::to_string(&events).unwrap()
    ))
}

/// Removes one thing at a time from the events and from the definition (a state, a
/// transition, an action, a guard, a target, an option) for as long as the two models still
/// disagree.
fn minimise(def: Definition, events: Vec<Event>, what: String) -> (Definition, Vec<Event>, String) {
    let mut scratch = Tally::default();
    minimise_with(def, events, what, &mut |d, e| {
        let mut replay = e.to_vec();
        drive(d, None, 0, &mut replay, &mut scratch).err()
    })
}

/// [`minimise`] for any way of deciding whether a definition and events still show the
/// problem (`Some` with its description) or not.
fn minimise_with(
    mut def: Definition,
    mut events: Vec<Event>,
    mut what: String,
    fails: &mut dyn FnMut(&Definition, &[Event]) -> Option<String>,
) -> (Definition, Vec<Event>, String) {
    let mut still_fails = |d: &Definition, e: &[Event]| -> Option<String> {
        Machine::compile(d).ok()?;
        fails(d, e)
    };
    // The disagreement is at the last event that was sent.
    for n in 0..events.len() {
        if let Some(w) = still_fails(&def, &events[..n]) {
            events.truncate(n);
            what = w;
            break;
        }
    }
    for _ in 0..50 {
        let mut progress = false;
        let mut i = 0;
        while i < events.len() {
            let mut fewer = events.clone();
            fewer.remove(i);
            match still_fails(&def, &fewer) {
                Some(w) => {
                    events = fewer;
                    what = w;
                    progress = true;
                }
                None => i += 1,
            }
        }
        let mut k = 0;
        loop {
            let mut json = serde_json::to_value(&def).unwrap();
            if !remove_nth(&mut json, &mut { k }) {
                break;
            }
            let reduced = serde_json::from_value::<Definition>(json)
                .ok()
                .and_then(|d| still_fails(&d, &events).map(|w| (d, w)));
            match reduced {
                Some((d, w)) => {
                    def = d;
                    what = w;
                    progress = true;
                }
                None => k += 1,
            }
        }
        if !progress {
            break;
        }
    }
    (def, events, what)
}

/// Removes the `n`-th removable part of a definition in JSON form: an element of any list,
/// or one of the optional fields. Counts down `n`; returns whether something was removed.
fn remove_nth(v: &mut serde_json::Value, n: &mut usize) -> bool {
    const OPTIONAL: [&str; 11] = [
        "guard",
        "initial",
        "deep",
        "internal",
        "meta",
        "data",
        "params",
        "unhandled",
        "context",
        "kind",
        "max_microsteps",
    ];
    match v {
        serde_json::Value::Array(items) => {
            for i in 0..items.len() {
                if *n == 0 {
                    items.remove(i);
                    return true;
                }
                *n -= 1;
            }
            items.iter_mut().any(|x| remove_nth(x, n))
        }
        serde_json::Value::Object(map) => {
            for key in OPTIONAL {
                if map.contains_key(key) {
                    if *n == 0 {
                        map.remove(key);
                        return true;
                    }
                    *n -= 1;
                }
            }
            map.values_mut().any(|x| remove_nth(x, n))
        }
        _ => false,
    }
}

/// Runs both models over one definition and compares them after every event. With `rng`,
/// events are generated and appended to `events`; without, `events` are replayed.
fn drive(
    def: &Definition,
    mut rng: Option<&mut Rng>,
    steps: usize,
    events: &mut Vec<Event>,
    tally: &mut Tally,
) -> Result<(), String> {
    let def = def.clone();
    let report = |what: &str, _events: &[Event]| what.to_string();
    let machine =
        Machine::compile(&def).map_err(|e| report(&format!("does not compile: {e}"), &[]))?;
    // The definition survives its own serialisation, and its hash with it.
    let again: Definition = serde_json::from_str(&serde_json::to_string(&def).unwrap()).unwrap();
    if again != def || Machine::compile(&again).unwrap().hash() != machine.hash() {
        return Err(report("definition does not round-trip", &[]));
    }
    let model = Model::new(&def);
    let interp = Interpreter::new(&model, machine.hash(), &common::guards);
    tally.definitions += 1;

    let (mut snapshot, mut wire) = match (
        machine.start_with(Value::Null, &common::guards),
        interp.start(&Value::Null),
    ) {
        (Ok(step), Ok((w, effects))) => {
            if machine.to_wire(&step.snapshot) != w {
                return Err(report(
                    &format!(
                        "start snapshots differ\nkernel:    {:?}\nreference: {w:?}",
                        machine.to_wire(&step.snapshot)
                    ),
                    &[],
                ));
            }
            if step.change.effects != effects {
                return Err(report(
                    &format!(
                        "start effects differ\nkernel:    {:?}\nreference: {effects:?}",
                        step.change.effects
                    ),
                    &[],
                ));
            }
            (step.snapshot, w)
        }
        (Err(e), Err(r)) if class(&e) == r => {
            tally.start_failed += 1;
            return Ok(());
        }
        (k, r) => {
            return Err(report(
                &format!(
                    "start outcomes differ\nkernel:    {:?}\nreference: {:?}",
                    k.map(|s| machine.to_wire(&s.snapshot)),
                    r.map(|x| x.0)
                ),
                &[],
            ));
        }
    };

    let replay = events.clone();
    let steps = if rng.is_some() { steps } else { replay.len() };
    let mut seen_timers: Vec<String> = Vec::new();
    for i in 0..steps {
        let event = match rng.as_deref_mut() {
            None => match replay.get(i) {
                Some(e) => e.clone(),
                None => break,
            },
            Some(rng) => {
                let mut event = generate::event(rng, &wire, i);
                // Sometimes send what only the kernel may produce: a timer event that may no
                // longer be pending, or a completion event.
                for t in wire.timers.values() {
                    if !seen_timers.contains(&t.event) {
                        seen_timers.push(t.event.clone());
                    }
                }
                if !seen_timers.is_empty() && rng.chance(6) {
                    event.name = rng.pick(&seen_timers).clone();
                } else if rng.chance(2) {
                    event.name = format!("done.state.{}", rng.pick(&["s1", "s2", "s3"]));
                }
                events.push(event.clone());
                event
            }
        };
        tally.events += 1;
        let before = snapshot.clone();
        let explanation = machine.explain(&before, &event, &common::guards);
        let kernel = machine.step_in_place(&mut snapshot, &event, &common::guards);
        // The explanation predicts the outcome and names the transitions of the first
        // microstep.
        let predicted: Vec<_> = explanation
            .candidates
            .iter()
            .filter(|c| c.verdict == Verdict::Taken)
            .map(|c| c.transition.clone())
            .collect();
        let expected = match &kernel {
            Ok(change) if change.handled => {
                let mut first = change.microsteps(&machine)[0].clone();
                first.sort_by(|a, b| (&a.source, a.index).cmp(&(&b.source, b.index)));
                let mut p = predicted.clone();
                p.sort_by(|a, b| (&a.source, a.index).cmp(&(&b.source, b.index)));
                if first != p {
                    return Err(report(
                        &format!("explain names {p:?}, the step took {first:?}"),
                        events,
                    ));
                }
                Outcome::Handled
            }
            Ok(_) => Outcome::Ignored,
            Err(StepError::Rejected { .. }) => Outcome::Rejected,
            Err(StepError::Done { .. }) => Outcome::Done,
            Err(_) => Outcome::Handled,
        };
        if explanation.outcome != expected {
            return Err(report(
                &format!(
                    "explain says {:?}, the step was {expected:?}",
                    explanation.outcome
                ),
                events,
            ));
        }
        if matches!(expected, Outcome::Ignored | Outcome::Rejected) && !predicted.is_empty() {
            return Err(report(
                "explain names a taken transition for an unhandled event",
                events,
            ));
        }
        let reference = interp.step(&wire, &event);
        match (kernel, reference) {
            (Ok(change), Ok((w, effects, handled))) => {
                let got = machine.to_wire(&snapshot);
                if std::env::var("FOLDSTATE_TRACE").is_ok() {
                    eprintln!(
                        "{} -> {:?} history {:?} microsteps {:?}",
                        event.name,
                        got.configuration,
                        got.history,
                        change.microsteps(&machine)
                    );
                }
                if got != w {
                    return Err(report(
                        &format!("snapshots differ\nkernel:    {got:?}\nreference: {w:?}"),
                        events,
                    ));
                }
                if change.effects != effects {
                    return Err(report(
                        &format!(
                            "effects differ\nkernel:    {:?}\nreference: {effects:?}",
                            change.effects
                        ),
                        events,
                    ));
                }
                if change.handled != handled {
                    return Err(report("handled flags differ", events));
                }
                if change.handled {
                    tally.handled += 1;
                } else {
                    tally.ignored += 1;
                }
                if change.microsteps(&machine).iter().any(|m| m.len() > 1) {
                    tally.multi += 1;
                }
                // The change is exactly the difference between the two snapshots.
                let mut redo = before.clone();
                change
                    .redo(&mut redo)
                    .map_err(|e| report(&format!("redo failed: {e}"), events))?;
                if redo != snapshot {
                    return Err(report(
                        "redoing the change does not give the new snapshot",
                        events,
                    ));
                }
                let mut undo = snapshot.clone();
                change
                    .undo(&mut undo)
                    .map_err(|e| report(&format!("undo failed: {e}"), events))?;
                if undo != before {
                    return Err(report(
                        &format!(
                            "undoing the change does not give the old snapshot\nchange: {change:?}\nbefore: {before:?}\nundone: {undo:?}"
                        ),
                        events,
                    ));
                }
                // The pure form agrees with the in-place form.
                let pure = machine
                    .step_with(&before, &event, &common::guards)
                    .map_err(|e| report(&format!("pure step failed: {e}"), events))?;
                if pure.snapshot != snapshot || pure.change != change {
                    return Err(report("step and step_in_place differ", events));
                }
                // Serialisation: wire snapshot through JSON and back.
                let json = serde_json::to_string(&got).unwrap();
                let parsed: WireSnapshot = serde_json::from_str(&json).unwrap();
                match machine.from_wire(&parsed) {
                    Ok(s) if s == snapshot => {}
                    Ok(_) => return Err(report("snapshot changes when serialised", events)),
                    Err(e) => return Err(report(&format!("own snapshot refused: {e}"), events)),
                }
                wire = w;
            }
            (Err(e), Err(r)) => {
                if class(&e) != r {
                    return Err(report(
                        &format!("errors differ: kernel {e:?}, reference {r:?}"),
                        events,
                    ));
                }
                if snapshot != before {
                    return Err(report(
                        &format!("a refused event changed the snapshot ({e})"),
                        events,
                    ));
                }
                match r {
                    RefError::Rejected => tally.rejected += 1,
                    RefError::Done => tally.done += 1,
                    RefError::Livelock => tally.livelock += 1,
                    RefError::Eval => tally.eval += 1,
                }
            }
            (k, r) => {
                return Err(report(
                    &format!(
                        "outcomes differ\nkernel:    {:?}\nreference: {:?}",
                        k.map(|c| c.handled),
                        r.map(|x| x.2)
                    ),
                    events,
                ));
            }
        }
    }
    Ok(())
}

#[test]
fn kernel_agrees_with_the_reference_model() {
    let cases = env("FOLDSTATE_CASES", 2000);
    let first = env("FOLDSTATE_SEED", 1);
    let mut tally = Tally::default();
    for seed in first..first + cases {
        if let Err(report) = case(seed, 40, &mut tally) {
            panic!("{report}");
        }
    }
    println!(
        "definitions {} (start failed {}), events {}: handled {}, ignored {}, rejected {}, done {}, livelock {}, eval {}; events with a multi-transition microstep {}",
        tally.definitions,
        tally.start_failed,
        tally.events,
        tally.handled,
        tally.ignored,
        tally.rejected,
        tally.done,
        tally.livelock,
        tally.eval,
        tally.multi
    );
    if cases >= 500 {
        // The run means little unless every kind of outcome occurred.
        assert!(tally.handled > 5 * cases, "few handled events");
        for (name, n) in [
            ("ignored", tally.ignored),
            ("rejected", tally.rejected),
            ("done", tally.done),
            ("livelock", tally.livelock),
            ("eval", tally.eval),
            ("multi-transition microsteps", tally.multi),
        ] {
            assert!(n > 0, "no {name} outcome in {cases} cases");
        }
    }
}

/// The reducer finds the one transition and the one event a problem depends on.
#[test]
fn a_failing_case_is_reduced_to_what_matters() {
    // Pretend the problem is: some state named in `tags` as "culprit" handles event `b`,
    // and `b` is sent after `a`.
    let mut fails = |d: &Definition, events: &[Event]| -> Option<String> {
        fn culprit(s: &foldstate::StateDef) -> bool {
            (s.tags.iter().any(|t| t == "culprit")
                && s.on.iter().any(|t| t.event.as_deref() == Some("b")))
                || s.states.iter().any(culprit)
        }
        let a = events.iter().position(|e| e.name == "a")?;
        let b = events.iter().rposition(|e| e.name == "b")?;
        (culprit(&d.root) && a < b).then(|| "still there".to_string())
    };
    let mut found = 0;
    for seed in 1..200u64 {
        let mut rng = Rng::new(seed);
        let mut def = generate::definition(&mut rng);
        // Plant the culprit in the first child that can hold transitions.
        let Some(victim) = def.root.states.iter_mut().find(|s| {
            !matches!(
                s.kind,
                Some(foldstate::Kind::Final | foldstate::Kind::History)
            )
        }) else {
            continue;
        };
        victim.tags.push("culprit".into());
        victim.on.push(foldstate::TransitionDef::stay("b"));
        let events: Vec<Event> = ["c", "a", "d", "a.x", "b", "c", "b", "d"]
            .into_iter()
            .map(Event::new)
            .collect();
        if fails(&def, &events).is_none() {
            continue;
        }
        found += 1;
        let (small, few, what) = minimise_with(def, events, "planted".into(), &mut fails);
        assert_eq!(what, "still there");
        assert_eq!(
            few.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
        // What is left: the root with as few children as a root may have, one of them the
        // culprit with its single transition.
        let json = serde_json::to_string(&small).unwrap();
        let transitions = json.matches("\"event\"").count();
        assert_eq!(transitions, 1, "seed {seed}: {json}");
        assert!(small.root.states.len() <= 2, "seed {seed}: {json}");
        assert!(
            small.root.states.iter().all(|s| s.states.is_empty()),
            "seed {seed}: {json}"
        );
    }
    assert!(found > 100);
}

/// Writes starting points for the fuzz targets (`fuzz/seeds`): generated definitions, runs
/// and logs. Run when the formats change:
/// `FOLDSTATE_WRITE_SEEDS=1 cargo test -p foldstate --test differential write_fuzz_seeds`.
#[test]
fn write_fuzz_seeds() {
    if std::env::var("FOLDSTATE_WRITE_SEEDS").is_err() {
        return;
    }
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/seeds");
    let log_definition: Definition = {
        let text = std::fs::read_to_string(dir.join("../fuzz_targets/log.rs")).unwrap();
        let start = text.find("r##\"").unwrap() + 4;
        let end = text.find("\"##").unwrap();
        serde_json::from_str(&text[start..end]).unwrap()
    };
    let log_machine = Machine::compile(&log_definition).unwrap();
    for seed in 1..=24u64 {
        let mut rng = Rng::new(seed);
        let def = generate::definition(&mut rng);
        let machine = Machine::compile(&def).unwrap();
        std::fs::write(
            dir.join(format!("definition/{seed}.json")),
            serde_json::to_string(&def).unwrap(),
        )
        .unwrap();
        let mut events = Vec::new();
        if let Ok(step) = machine.start_with(Value::Null, &common::guards) {
            let mut snapshot = step.snapshot;
            for i in 0..12 {
                let event = generate::event(&mut rng, &machine.to_wire(&snapshot), i);
                let _ = machine.step_in_place(&mut snapshot, &event, &common::guards);
                events.push(event);
            }
        }
        std::fs::write(
            dir.join(format!("step/{seed}.json")),
            serde_json::to_string(&serde_json::json!({"definition": def, "events": events}))
                .unwrap(),
        )
        .unwrap();
        // Logs of the fixed machine the `log` target uses, with forks and restores.
        let mut log = foldstate::Log::start(&log_machine, Value::Null).unwrap();
        for _ in 0..(3 + rng.below(8)) {
            let branch = rng.below(log.branches().count()) as u32;
            let head = log.head_seq(branch).unwrap();
            match rng.below(10) {
                0 => {
                    let _ = log.fork(branch, 1 + rng.below(head as usize) as u64);
                }
                1 => {
                    let _ = log.restore(&log_machine, branch, 1 + rng.below(head as usize) as u64);
                }
                _ => {
                    let name = *rng.pick(&["go", "back", "after.a.a1:after:0"]);
                    let _ = log.append(&log_machine, branch, Event::new(name).at(seed as i64));
                }
            }
        }
        std::fs::write(
            dir.join(format!("log/{seed}.json")),
            serde_json::to_string(&log.to_wire(&log_machine).unwrap()).unwrap(),
        )
        .unwrap();
    }
}

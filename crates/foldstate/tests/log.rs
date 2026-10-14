// SPDX-License-Identifier: Apache-2.0
//! Properties of the log on generated definitions: state at N is a fold for any checkpoint
//! interval, the cursor moves both ways, forks share their prefix, restore gives the old
//! state, verification accepts what the engine wrote and catches what it did not, and the
//! serialised form reads back to the same log.

mod common;

use common::generate::{self, Rng};
use foldstate::{
    Definition, EntryKind, Event, Log, LogError, MAIN, Machine, Snapshot, Value, WireLog, WireOp,
    WireSnapshot,
};
use serde_json::json;

fn env(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// A random log over a random definition: appends, forks and restores.
fn build(seed: u64, interval: u64) -> Option<(Definition, Machine, Log)> {
    let mut rng = Rng::new(seed);
    let def = generate::definition(&mut rng);
    let machine = Machine::compile(&def).expect("generated definitions compile");
    let mut log = Log::start_with(&machine, Value::Null, &common::guards).ok()?;
    log.set_checkpoint_interval(interval);
    for step in 0..60 {
        let branches = log.branches().count() as u32;
        let branch = rng.below(branches as usize) as u32;
        let head = log.head_seq(branch).unwrap();
        match rng.below(100) {
            0..=5 if branches < 5 => {
                let at = 1 + rng.below(head as usize) as u64;
                log.fork(branch, at).unwrap();
            }
            6..=11 => {
                let to = 1 + rng.below(head as usize) as u64;
                log.restore(&machine, branch, to).unwrap();
            }
            _ => {
                let wire = machine.to_wire(log.head(branch).unwrap());
                let event = generate::event(&mut rng, &wire, step);
                // Refused events are simply not recorded.
                let _ = log.append_with(&machine, branch, event, &common::guards);
            }
        }
    }
    Some((def, machine, log))
}

/// The snapshot after `seq` by applying every entry from the first, with no checkpoint.
fn fold(log: &Log, branch: u32, seq: u64) -> Snapshot {
    let mut cursor = log.cursor(branch, 1).unwrap();
    while cursor.seq() < seq {
        cursor.forward().unwrap();
    }
    cursor.snapshot().clone()
}

#[test]
fn log_properties_hold_on_generated_logs() {
    let cases = env("FOLDSTATE_CASES", 300);
    let first = env("FOLDSTATE_SEED", 1);
    let (mut logs, mut entries, mut forks, mut restores) = (0u64, 0u64, 0u64, 0u64);
    for seed in first..first + cases {
        let interval = [1, 2, 7, 256][(seed % 4) as usize];
        let Some((def, machine, log)) = build(seed, interval) else {
            continue;
        };
        logs += 1;
        let context = |what: &str| {
            format!(
                "seed {seed}: {what}\n{}",
                serde_json::to_string(&def).unwrap()
            )
        };

        // Verification accepts the log, with the host's guards and with the recorded results.
        log.verify_with(&machine, &common::guards)
            .unwrap_or_else(|e| panic!("{}", context(&format!("verify with host: {e}"))));
        log.verify(&machine).unwrap_or_else(|e| {
            panic!("{}", context(&format!("verify from recorded guards: {e}")))
        });

        for b in log.branches() {
            forks += u64::from(b.parent.is_some());
            let head = log.head_seq(b.id).unwrap();
            assert_eq!(b.head, head);
            // State at N, however it is computed.
            let mut cursor = log.cursor(b.id, 1).unwrap();
            let mut states = vec![cursor.snapshot().clone()];
            for seq in 2..=head {
                let entry = cursor.forward().unwrap();
                assert_eq!(entry.seq, seq);
                restores += u64::from(matches!(entry.kind, EntryKind::Restore { .. }));
                states.push(cursor.snapshot().clone());
            }
            entries += head;
            assert!(matches!(cursor.forward(), Err(LogError::AtEnd)));
            assert_eq!(
                cursor.snapshot(),
                log.head(b.id).unwrap(),
                "{}",
                context("fold != head")
            );
            for seq in 1..=head {
                let at = log.state_at(b.id, seq).unwrap();
                assert_eq!(
                    at,
                    states[(seq - 1) as usize],
                    "{}",
                    context("state_at != fold")
                );
                machine
                    .from_wire(&machine.to_wire(&at))
                    .unwrap_or_else(|e| {
                        panic!("{}", context(&format!("illegal state at {seq}: {e}")))
                    });
                // A restore entry leaves exactly the state it names.
                if let EntryKind::Restore { to } = log.entry(b.id, seq).unwrap().kind {
                    assert_eq!(
                        at,
                        states[(to - 1) as usize],
                        "{}",
                        context("restore != target")
                    );
                }
            }
            // And back again, one entry at a time.
            for seq in (1..head).rev() {
                cursor.back().unwrap();
                assert_eq!(
                    cursor.snapshot(),
                    &states[(seq - 1) as usize],
                    "{}",
                    context("back")
                );
            }
            assert!(matches!(cursor.back(), Err(LogError::AtEnd)));
            // Seeking far and near gives the same.
            let mut seeker = log.cursor(b.id, head).unwrap();
            for target in [1, head / 2 + 1, head, 1.max(head / 3)] {
                seeker.seek(target).unwrap();
                assert_eq!(seeker.snapshot(), &states[(target - 1) as usize]);
            }
            // A fork shares its parent's entries up to the fork point, and no more.
            if let Some((parent, at)) = b.parent {
                for seq in 1..=at {
                    assert!(std::ptr::eq(
                        log.entry(b.id, seq).unwrap(),
                        log.entry(parent, seq).unwrap()
                    ));
                }
                let d = log.diff(&machine, (b.id, at), (parent, at)).unwrap();
                assert!(d.is_empty() && d.shared == at);
                if head > at && log.head_seq(parent).unwrap() > at {
                    let d = log.diff(&machine, (b.id, head), (parent, at + 1)).unwrap();
                    assert_eq!(d.shared, at);
                }
                assert_eq!(fold(&log, b.id, at), log.state_at(parent, at).unwrap());
            }
        }

        // The serialised form reads back to the same log.
        let wire = log.to_wire(&machine).unwrap();
        let json = serde_json::to_string(&wire).unwrap();
        let parsed: WireLog = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, wire, "{}", context("wire log changes through JSON"));
        let read = Log::from_wire(&machine, &parsed)
            .unwrap_or_else(|e| panic!("{}", context(&format!("own log refused: {e}"))));
        read.verify(&machine)
            .unwrap_or_else(|e| panic!("{}", context(&format!("read log does not verify: {e}"))));
        for b in log.branches() {
            assert_eq!(read.head(b.id).unwrap(), log.head(b.id).unwrap());
        }
        // Following the serialised entries without the definition gives the same states,
        // forwards and backwards.
        let main = &wire.branches[0].entries;
        let mut replay = WireSnapshot::blank(machine.hash());
        let sorted = |seq: u64| {
            let mut w = machine.to_wire(&log.state_at(MAIN, seq).unwrap());
            w.configuration.sort();
            w
        };
        for e in main {
            replay.redo(e).unwrap();
            assert_eq!(replay, sorted(e.seq), "{}", context("wire replay forward"));
        }
        for e in main.iter().skip(1).rev() {
            replay.undo(e).unwrap();
            assert_eq!(
                replay,
                sorted(e.seq - 1),
                "{}",
                context("wire replay backward")
            );
        }
        assert_eq!(read.to_wire(&machine).unwrap(), wire);
    }
    println!("logs {logs}, entries {entries}, forks {forks}, restores {restores}");
    assert!(logs > cases / 2 && forks > 0 && restores > 0);
}

fn traffic() -> Machine {
    let def: Definition = serde_json::from_value(json!({
        "schema": "foldstate/1", "id": "light", "context": {"cycles": 0},
        "root": {"id": "light", "states": [
            {"id": "red", "on": [{"event": "next", "target": ["green"]}],
             "after": [{"delay_ms": 30000, "target": ["green"]}]},
            {"id": "green", "on": [{"event": "next", "target": ["amber"]}]},
            {"id": "amber", "on": [{"event": "next", "target": ["red"], "actions": [
                {"assign": {"path": "/cycles", "value": {"add": [{"ctx": "/cycles"}, {"lit": 1}]}}}
            ]}]}
        ]}
    }))
    .unwrap();
    Machine::compile(&def).unwrap()
}

#[test]
fn fork_restore_and_diff_on_a_small_machine() {
    let m = traffic();
    let mut log = Log::start(&m, Value::Null).unwrap();
    for _ in 0..4 {
        log.append(&m, MAIN, Event::new("next")).unwrap();
    }
    // 1 start (red), 2 green, 3 amber, 4 red (cycles 1), 5 green
    assert_eq!(m.configuration(log.head(MAIN).unwrap()), ["green"]);
    assert_eq!(log.head(MAIN).unwrap().context(), &json!({"cycles": 1}));

    // A refused event leaves no trace.
    assert!(matches!(
        log.append(&m, MAIN, Event::new("stop")),
        Err(LogError::Step(_))
    ));
    assert_eq!(log.head_seq(MAIN).unwrap(), 5);

    // Fork from "amber" and go another way.
    let branch = log.fork(MAIN, 3).unwrap();
    assert_eq!(m.configuration(log.head(branch).unwrap()), ["amber"]);
    assert_eq!(log.head_seq(branch).unwrap(), 3);
    log.append(&m, branch, Event::new("next")).unwrap();
    assert_eq!(log.head_seq(branch).unwrap(), 4);
    assert_eq!(log.head_seq(MAIN).unwrap(), 5, "the parent is unaffected");

    let d = log.diff(&m, (MAIN, 5), (branch, 4)).unwrap();
    assert_eq!(d.shared, 3);
    assert_eq!(
        (d.only_left.as_slice(), d.only_right.as_slice()),
        (&["green".to_string()][..], &["red".to_string()][..])
    );
    assert_eq!(
        d.timers,
        ["red:after:0"],
        "red's timer is pending on the fork only"
    );

    // Restore: back to the first green, as a new entry.
    let entry = log.restore(&m, MAIN, 2).unwrap();
    assert_eq!((entry.seq, &entry.kind), (6, &EntryKind::Restore { to: 2 }));
    assert_eq!(log.head(MAIN).unwrap(), &log.state_at(MAIN, 2).unwrap());
    assert_eq!(log.head(MAIN).unwrap().context(), &json!({"cycles": 0}));
    assert_eq!(log.entry(MAIN, 5).unwrap().seq, 5, "nothing was removed");
    log.verify(&m).unwrap();

    // Restoring into "red" asks the host to schedule red's timer again.
    let entry = log.restore(&m, MAIN, 1).unwrap();
    assert!(
        matches!(entry.change.effects.as_slice(), [foldstate::Effect::Schedule { id, .. }] if id == "red:after:0")
    );
}

#[test]
fn verification_catches_a_changed_log_and_a_changed_definition() {
    let m = traffic();
    let mut log = Log::start(&m, Value::Null).unwrap();
    for _ in 0..3 {
        log.append(&m, MAIN, Event::new("next")).unwrap();
    }
    let wire = log.to_wire(&m).unwrap();

    // An entry whose recorded change is not what the definition gives.
    let mut forged = wire.clone();
    for op in &mut forged.branches[0].entries[2].ops {
        if let WireOp::Enter(state) = op {
            *state = "red".into();
        }
    }
    match Log::from_wire(&m, &forged) {
        // Entry 3 now enters red; entry 4 then tries to leave amber.
        Ok(read) => assert!(matches!(
            read.verify(&m),
            Err(LogError::Diverged {
                branch: 0,
                seq: 3,
                ..
            })
        )),
        Err(e) => assert!(
            matches!(e, LogError::Malformed(_) | LogError::Wire(_)),
            "{e}"
        ),
    }

    // An effect that was never produced.
    let mut forged = wire.clone();
    forged.branches[0].entries[1]
        .effects
        .push(foldstate::Effect::Emit {
            name: "pay".into(),
            params: Value::Null,
        });
    let read = Log::from_wire(&m, &forged).unwrap();
    assert_eq!(
        read.verify(&m),
        Err(LogError::Diverged {
            branch: 0,
            seq: 2,
            what: "effects"
        })
    );

    // A gap in the sequence.
    let mut forged = wire.clone();
    forged.branches[0].entries.remove(1);
    assert!(matches!(
        Log::from_wire(&m, &forged),
        Err(LogError::Malformed(_))
    ));

    // Another definition: same shape, one transition different.
    let mut other = m.definition().clone();
    other.root.states[1].on[0].target = vec!["red".into()];
    let other = Machine::compile(&other).unwrap();
    assert_ne!(other.hash(), m.hash());
    assert_eq!(
        Log::from_wire(&other, &wire).err(),
        Some(LogError::MachineMismatch)
    );
    assert_eq!(log.verify(&other), Err(LogError::MachineMismatch));
    assert!(matches!(
        log.append(&other, MAIN, Event::new("next")),
        Err(LogError::MachineMismatch)
    ));
}

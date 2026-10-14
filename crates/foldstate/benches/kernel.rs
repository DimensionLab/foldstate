// SPDX-License-Identifier: Apache-2.0
//! Timings of the kernel and the log. Run with `cargo bench -p foldstate`.
//!
//! No benchmarking framework: each measurement runs a fixed workload a few times and
//! reports the best time per operation, which is the least disturbed one.

use std::hint::black_box;
use std::time::Instant;

use foldstate::{Definition, Event, Log, MAIN, Machine, NoGuards, Value};
use serde_json::json;

/// A machine with `regions` parallel regions, each a nine-state job lifecycle. With
/// `derived`, a region leaves `waiting` by itself once the region before it has succeeded.
fn lifecycle(regions: usize, derived: bool) -> Definition {
    let region = |i: usize| {
        let ev = |name: &str| format!("r{i}.{name}");
        let mut waiting = json!({"id": "waiting", "on": [
            {"event": ev("queue"), "target": ["queued"]},
            {"event": ev("skip"), "target": ["skipped"]}
        ]});
        if derived && i > 0 {
            waiting["on"] = json!([
                {"guard": {"in": format!("#r{}.succeeded", i - 1)}, "target": ["queued"]},
                {"event": ev("skip"), "target": ["skipped"]}
            ]);
        }
        json!({"id": format!("r{i}"), "states": [
            {"id": "pending", "on": [{"event": ev("plan"), "target": ["waiting"]}]},
            waiting,
            {"id": "queued", "on": [
                {"event": ev("run"), "target": ["running"]},
                {"event": ev("cache"), "target": ["cached"]}
            ]},
            {"id": "running", "on": [
                {"event": ev("progress"), "actions": [
                    {"assign": {"path": format!("/r{i}/progress"), "value": {"event": "/fraction"}}}
                ]},
                {"event": ev("ok"), "target": ["succeeded"]},
                {"event": ev("fail"), "target": ["failed"]},
                {"event": ev("cancel"), "target": ["cancelled"]}
            ]},
            {"id": "succeeded", "tags": ["success"], "on": [{"event": ev("reset"), "target": ["pending"]}]},
            {"id": "failed", "on": [{"event": ev("retry"), "target": ["queued"]}]},
            {"id": "cached", "tags": ["success"]},
            {"id": "skipped"},
            {"id": "cancelled"}
        ]})
    };
    serde_json::from_value(json!({
        "schema": "foldstate/1", "id": "bench",
        "root": {"id": "bench", "kind": "parallel",
                 "states": (0..regions).map(region).collect::<Vec<_>>()}
    }))
    .expect("definition")
}

/// One full lifecycle of region `i`: plan, queue, run, `progress` updates, ok, reset.
fn cycle(i: usize, progress: usize, derived: bool) -> Vec<Event> {
    let ev = |name: &str| Event::new(format!("r{i}.{name}"));
    let mut out = vec![ev("plan")];
    if !(derived && i > 0) {
        out.push(ev("queue"));
    }
    out.push(ev("run"));
    for p in 0..progress {
        out.push(ev("progress").with_data(json!({"fraction": p as f64 / progress as f64})));
    }
    out.push(ev("ok"));
    out
}

/// Best of `rounds` runs of `f`, in nanoseconds per operation.
fn best<F: FnMut() -> u64>(rounds: usize, mut f: F) -> f64 {
    let mut best = f64::INFINITY;
    for _ in 0..rounds {
        let start = Instant::now();
        let ops = f();
        best = best.min(start.elapsed().as_nanos() as f64 / ops as f64);
    }
    best
}

fn row(name: &str, ns: f64) {
    let per_second = 1e9 / ns;
    println!("{name:<58} {ns:>10.1} ns   {:>8.2} M/s", per_second / 1e6);
}

fn step_throughput(regions: usize, derived: bool) {
    let def = lifecycle(regions, derived);
    let machine = Machine::compile(&def).expect("compiles");
    // Events for every region, interleaved, with resets so the workload can repeat.
    let mut events: Vec<Event> = Vec::new();
    if derived {
        // A chain: each region waits for the one before, so regions run in order.
        for i in 0..regions {
            events.push(Event::new(format!("r{i}.plan")));
        }
        for i in 0..regions {
            if i == 0 {
                events.push(Event::new("r0.queue"));
            }
            events.push(Event::new(format!("r{i}.run")));
            events.push(Event::new(format!("r{i}.progress")).with_data(json!({"fraction": 0.5})));
            events.push(Event::new(format!("r{i}.ok")));
        }
    } else {
        for i in 0..regions {
            events.extend(cycle(i, 2, false));
            events.push(Event::new(format!("r{i}.reset")));
        }
    }
    let start = machine.start(Value::Null).expect("starts").snapshot;
    let rounds = (200_000 / events.len()).max(1);
    let ns = best(5, || {
        let mut n = 0u64;
        for _ in 0..rounds {
            let mut snapshot = start.clone();
            for e in &events {
                black_box(
                    machine
                        .step_in_place(&mut snapshot, e, &NoGuards)
                        .expect("accepted"),
                );
                n += 1;
            }
        }
        n
    });
    let kind = if derived {
        "readiness by guard"
    } else {
        "all events sent"
    };
    row(&format!("step, {regions} regions x 9 states, {kind}"), ns);
}

fn compile_time(regions: usize) {
    let def = lifecycle(regions, true);
    let ns = best(5, || {
        black_box(Machine::compile(&def).expect("compiles"));
        1
    });
    println!(
        "{:<58} {:>10.3} ms",
        format!("compile, {regions} regions ({} states)", regions * 10 + 1),
        ns / 1e6
    );
}

fn fold_a_million() {
    const ENTRIES: usize = 1_000_000;
    let regions = 10;
    let def = lifecycle(regions, false);
    let machine = Machine::compile(&def).expect("compiles");
    // Mostly progress updates, as a real run would be.
    let mut events: Vec<Event> = Vec::with_capacity(ENTRIES);
    'fill: loop {
        for i in 0..regions {
            for e in cycle(i, 40, false) {
                events.push(e);
                if events.len() == ENTRIES - 1 {
                    break 'fill;
                }
            }
            events.push(Event::new(format!("r{i}.reset")));
        }
    }
    events.truncate(ENTRIES - 1);

    let start = Instant::now();
    let mut log = Log::start(&machine, Value::Null).expect("starts");
    for e in &events {
        log.append(&machine, MAIN, e.clone()).expect("accepted");
    }
    let append = start.elapsed();
    let head = log.head_seq(MAIN).expect("head");
    println!("\nlog of {head} entries, {regions} regions");
    println!(
        "{:<58} {:>10.1} ms   {:>8.1} ns/entry",
        "append (step + record)",
        append.as_secs_f64() * 1e3,
        append.as_nanos() as f64 / head as f64
    );

    let start = Instant::now();
    let mut cursor = log.cursor(MAIN, 1).expect("cursor");
    while cursor.forward().is_ok() {}
    let fold = start.elapsed();
    assert_eq!(cursor.snapshot(), log.head(MAIN).expect("head"));
    println!(
        "{:<58} {:>10.1} ms   {:>8.1} ns/entry",
        "fold: apply every entry from the first",
        fold.as_secs_f64() * 1e3,
        fold.as_nanos() as f64 / head as f64
    );

    let start = Instant::now();
    while cursor.back().is_ok() {}
    let back = start.elapsed();
    println!(
        "{:<58} {:>10.1} ms   {:>8.1} ns/entry",
        "unfold: undo every entry back to the first",
        back.as_secs_f64() * 1e3,
        back.as_nanos() as f64 / head as f64
    );

    let start = Instant::now();
    log.verify(&machine).expect("verifies");
    let verify = start.elapsed();
    println!(
        "{:<58} {:>10.1} ms   {:>8.1} ns/entry",
        "verify: recompute and compare every entry",
        verify.as_secs_f64() * 1e3,
        verify.as_nanos() as f64 / head as f64
    );

    // Random access with the default checkpoint interval (256).
    let mut x = 0x9E37_79B9_7F4A_7C15u64;
    let samples = 2000;
    let start = Instant::now();
    for _ in 0..samples {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        black_box(log.state_at(MAIN, 1 + x % head).expect("state"));
    }
    let at = start.elapsed();
    println!(
        "{:<58} {:>10.2} us   per call",
        "state_at(random entry), checkpoint every 256",
        at.as_secs_f64() * 1e6 / samples as f64
    );

    let start = Instant::now();
    let branch = log.fork(MAIN, head / 2).expect("fork");
    let fork = start.elapsed();
    black_box(branch);
    println!(
        "{:<58} {:>10.2} us",
        "fork at the middle",
        fork.as_secs_f64() * 1e6
    );

    let start = Instant::now();
    let wire = log.to_wire(&machine).expect("wire");
    let json = serde_json::to_string(&wire).expect("json");
    let ser = start.elapsed();
    println!(
        "{:<58} {:>10.1} ms   {:>8.1} bytes/entry",
        "serialise to JSON",
        ser.as_secs_f64() * 1e3,
        json.len() as f64 / head as f64
    );
    let start = Instant::now();
    let parsed: foldstate::WireLog = serde_json::from_str(&json).expect("parse");
    let read = Log::from_wire(&machine, &parsed).expect("read");
    let de = start.elapsed();
    black_box(read);
    println!(
        "{:<58} {:>10.1} ms",
        "parse from JSON and rebuild",
        de.as_secs_f64() * 1e3
    );
}

fn main() {
    println!(
        "{:<58} {:>13}   {:>12}",
        "workload", "per event", "throughput"
    );
    for regions in [1, 10, 100, 1000] {
        step_throughput(regions, false);
    }
    for regions in [10, 100, 1000] {
        step_throughput(regions, true);
    }
    println!();
    for regions in [10, 100, 1000] {
        compile_time(regions);
    }
    fold_a_million();
}

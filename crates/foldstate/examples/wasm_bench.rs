// SPDX-License-Identifier: Apache-2.0
//! One fixed workload for comparing native and WebAssembly speed: 100 parallel regions,
//! 200 000 events appended to a log, then the log folded from its first entry.
//!
//! Native: `cargo run -p foldstate --release --example wasm_bench` prints the time.
//! WebAssembly: build with `--target wasm32-unknown-unknown` and time the exported `main`
//! from the host (`examples/wasm_bench.mjs` does it in Node). The program reads no clock
//! itself when built for WebAssembly, where there is none.

use std::hint::black_box;

use foldstate::{Definition, Event, Log, MAIN, Machine, Value};
use serde_json::json;

const REGIONS: usize = 100;
const EVENTS: usize = 200_000;

fn definition() -> Definition {
    let region = |i: usize| {
        let ev = |name: &str| format!("r{i}.{name}");
        json!({"id": format!("r{i}"), "states": [
            {"id": "pending", "on": [{"event": ev("plan"), "target": ["waiting"]}]},
            {"id": "waiting", "on": [{"event": ev("queue"), "target": ["queued"]}]},
            {"id": "queued", "on": [{"event": ev("run"), "target": ["running"]}]},
            {"id": "running", "on": [
                {"event": ev("progress"), "actions": [
                    {"assign": {"path": format!("/r{i}/progress"), "value": {"event": "/fraction"}}}
                ]},
                {"event": ev("ok"), "target": ["succeeded"]}
            ]},
            {"id": "succeeded", "on": [{"event": ev("reset"), "target": ["pending"]}]}
        ]})
    };
    serde_json::from_value(json!({
        "schema": "foldstate/1", "id": "bench",
        "root": {"id": "bench", "kind": "parallel",
                 "states": (0..REGIONS).map(region).collect::<Vec<_>>()}
    }))
    .expect("definition")
}

fn workload() -> u64 {
    let machine = Machine::compile(&definition()).expect("compiles");
    let mut log = Log::start(&machine, Value::Null).expect("starts");
    let names = [
        "plan", "queue", "run", "progress", "progress", "progress", "ok", "reset",
    ];
    let mut n = 0;
    'fill: loop {
        for i in 0..REGIONS {
            for name in names {
                let mut event = Event::new(format!("r{i}.{name}"));
                if name == "progress" {
                    event = event.with_data(json!({"fraction": 0.5}));
                }
                log.append(&machine, MAIN, event).expect("accepted");
                n += 1;
                if n == EVENTS {
                    break 'fill;
                }
            }
        }
    }
    let mut cursor = log.cursor(MAIN, 1).expect("cursor");
    while cursor.forward().is_ok() {}
    assert_eq!(cursor.snapshot(), log.head(MAIN).expect("head"));
    log.head_seq(MAIN).expect("head")
}

fn main() {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let mut best = f64::INFINITY;
        for _ in 0..5 {
            let start = std::time::Instant::now();
            black_box(workload());
            best = best.min(start.elapsed().as_secs_f64());
        }
        println!(
            "native: {:.1} ms for {EVENTS} appends and a fold ({:.0} ns per event)",
            best * 1e3,
            best * 1e9 / EVENTS as f64
        );
    }
    #[cfg(target_arch = "wasm32")]
    {
        black_box(workload());
    }
}

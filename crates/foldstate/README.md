<!-- SPDX-License-Identifier: Apache-2.0 -->
# foldstate

Statecharts as data, with a log you can replay, fork and step through in both directions.

A machine is defined in JSON (or built in code): hierarchical and parallel states, history,
guards, delayed transitions. The kernel is a pure function,
`(snapshot, event) -> (snapshot, effects)`: it performs no I/O, reads no clock and gives the
same result on every platform. Every event becomes a log entry that records what changed, so
the state at any point is the fold of the entries before it.

Experimental. The API and the serialised formats can change before 1.0.

- `no_std` + `alloc`, no `unsafe`, no async runtime. Dependencies: `serde`, `serde_json`.
- Builds for `wasm32-unknown-unknown`.
- Minimum Rust version: 1.89.
- Licence: Apache-2.0.

## When not to use this

- **The machine is fixed when you compile.** A state machine written in Rust (for example
  with [statig](https://crates.io/crates/statig)) handles an event in under a nanosecond.
  foldstate takes about 0.2 to 0.6 microseconds: roughly 500 times slower. It pays for
  reading the machine from data and for recording every change.
- **You do not need the log.** If nobody will replay, step back, fork or audit, the record
  of every change is cost without benefit.
- **You need workflows as code** (loops, awaits, calls into your program between steps).
  This is a statechart interpreter, not a durable execution engine.
- **You need an exact SCXML or XState runtime.** The semantics follow SCXML's algorithm
  with stated differences, and there is no scripting data model.
- **Binary size is tight.** The whole crate adds about 0.4 to 0.7 MB to a WebAssembly
  binary before compression.

## Use it

```toml
[dependencies]
foldstate = "0.1"
```

```rust
use foldstate::{Definition, Effect, Event, Log, MAIN, Machine, NoGuards, Outcome, Value};

// A definition is data.
let definition: Definition = serde_json::from_str(r##"{
  "schema": "foldstate/1",
  "id": "upload",
  "context": {"attempt": 0},
  "root": {"id": "upload", "states": [
    {"id": "idle", "on": [{"event": "start", "target": ["sending"]}]},
    {"id": "sending",
     "entry": [{"assign": {"path": "/attempt", "value": {"add": [{"ctx": "/attempt"}, {"lit": 1}]}}}],
     "invoke": [{"name": "http.put", "params": {"event": "/url"}}],
     "after": [{"delay_ms": 30000, "target": ["failed"]}],
     "on": [
       {"event": "ok", "target": ["done"]},
       {"event": "error", "target": ["sending"],
        "guard": {"cmp": {"left": {"ctx": "/attempt"}, "op": "lt", "right": {"lit": 3}}}},
       {"event": "error", "target": ["failed"]}
     ]},
    {"id": "failed", "on": [{"event": "start", "target": ["sending"]}]},
    {"id": "done", "kind": "final"}
  ]}
}"##)?;
let machine = Machine::compile(&definition)?;

// A log is the only writer. Each append is one step of the kernel.
let mut log = Log::start(&machine, Value::Null)?;
let url = serde_json::json!({"url": "https://files.example.com/a"});
let entry = log.append(&machine, MAIN, Event::new("start").with_data(url).at(1_000))?;

// Effects are data. Your code performs them and reports back with events.
assert_eq!(entry.change.effects, [
    Effect::Schedule {
        id: "sending:after:0".into(),
        event: "after.sending:after:0".into(),
        delay_ms: 30_000,
        due_ms: Some(31_000),
    },
    Effect::Start {
        id: "sending:invoke:0".into(),
        name: "http.put".into(),
        params: "https://files.example.com/a".into(),
    },
]);
log.append(&machine, MAIN, Event::new("error"))?;   // entry 3: second attempt
log.append(&machine, MAIN, Event::new("error"))?;   // entry 4: third attempt
log.append(&machine, MAIN, Event::new("error"))?;   // entry 5: gives up
assert!(machine.is_in(log.head(MAIN)?, "failed"));

// State at any entry, and stepping in both directions.
assert!(machine.is_in(&log.state_at(MAIN, 3)?, "sending"));
let mut cursor = log.cursor(MAIN, 5)?;
cursor.back()?;
assert_eq!(cursor.snapshot().context()["attempt"], 3);

// Why did the last error not retry?
let why = machine.explain(cursor.snapshot(), &Event::new("error"), &NoGuards);
assert_eq!(why.outcome, Outcome::Handled);
assert_eq!(why.candidates[0].guard.as_ref().unwrap().result, Some(false));

// Fork from entry 4 and try something else. The original branch is untouched.
let branch = log.fork(MAIN, 4)?;
log.append(&machine, branch, Event::new("ok"))?;
assert!(log.head(branch)?.is_done());
assert_eq!(log.diff(&machine, (MAIN, 5), (branch, 5))?.only_left, ["failed"]);

// Go back without losing history: restore appends an entry.
log.restore(&machine, MAIN, 2)?;
assert_eq!(log.head_seq(MAIN)?, 6);

// A log can be checked against its definition, and stored as JSON.
log.verify(&machine)?;
let json = serde_json::to_string(&log.to_wire(&machine)?)?;
# let _ = json;
# Ok::<(), Box<dyn std::error::Error>>(())
```

## What you get

| | |
|---|---|
| `Machine::compile` | Validates a definition and reports the first problem with the place it is in. |
| `Machine::start`, `step`, `step_in_place` | The kernel. A refused event changes nothing. |
| `Log::append`, `state_at`, `cursor` | Record events; read the state at any entry; move one entry forward or back. |
| `Log::fork`, `diff` | Continue from any entry on a new branch that shares the earlier entries; compare two positions. |
| `Log::restore` | Make an earlier state current again by appending an entry. Nothing is removed. |
| `Log::verify` | Recompute every entry from the definition and report the first one that differs. |
| `Machine::explain`, `enabled` | Which transitions an event would take, which it would not and why; which events are accepted now. |
| `Machine::states`, `transitions` | The graph, for drawing it. |
| `to_wire`, `from_wire` | Snapshots, entries and logs as JSON. State references are paths, not numbers. |
| `WireSnapshot::redo`, `undo` | Follow a serialised log entry by entry without the definition. |

## Effects, timers and work owned by a state

The kernel never does anything outside the snapshot. It returns effects:

| Effect | The host should |
|---|---|
| `emit {name, params}` | Do whatever `name` means. |
| `schedule {id, event, delay_ms, due_ms?}` | Send `event` after the delay. |
| `cancel {id}` | Forget that timer. |
| `start {id, name, params}` | Start the work a state declared with `invoke`. |
| `stop {id}` | Stop it: the state was left. |

Results come back as ordinary events. Time is data: pass the event's time in `Event::at` and
timers get a due time; the kernel reads no clock, so a replay never depends on when it runs.

## Guards from your code

A definition can refer to a guard by name: `{"named": {"name": "has_quota"}}`. Pass an
implementation of `Guards` (a closure works) to `step_with`, `append_with` or `explain`. A
guard must be a pure function of its arguments. Its results are recorded in the log entry,
so `Log::verify` and `Log::state_at` work without your code.

## Events that nothing handles

By default an event that enables no transition is an error (`StepError::Rejected`) and is
not recorded. List descriptors under `ignore` in the definition, or set
`"unhandled": "ignore"`, to accept such events without effect.

## Formats and semantics

- [Semantics](docs/semantics.md): how an event is processed, in which order actions run, what
  a log entry records, and the differences from W3C SCXML.
- JSON Schemas: [definition](docs/schema/definition.schema.json),
  [snapshot](docs/schema/snapshot.schema.json), [log entry](docs/schema/entry.schema.json),
  [log](docs/schema/log.schema.json).
- A log names its definition by the SHA-256 of the definition's canonical JSON (RFC 8785).
  A machine compiled from a different definition refuses the log. `foldstate::canon` has the
  canonical form and the hash, for hosts that chain entries.

## Performance

Measured on an Apple M1 Max, one thread, Rust 1.99, release build. Reproduce with
`cargo bench -p foldstate`; the size and WebAssembly figures come from the `size_*` and
`wasm_bench` examples.

One event (`step_in_place`), each region a nine-state job lifecycle:

| Parallel regions | Every event sent | Readiness derived by guards |
|---|---|---|
| 1 | 0.33 µs | |
| 10 | 0.39 µs | 0.46 µs |
| 100 | 0.48 µs | 0.58 µs |
| 1000 | 0.61 µs | 0.84 µs |

A log of 1 000 000 entries (10 regions, mostly progress updates):

| Operation | Time |
|---|---|
| Append (step and record) | 0.88 s, 0.88 µs per entry |
| Fold: apply every entry from the first | 0.24 s |
| Undo every entry back to the first | 0.24 s |
| Verify: recompute and compare every entry | 0.65 s |
| State at a random entry, checkpoint every 256 | 34 µs |
| Fork in the middle | 13 µs |
| Write as JSON (194 bytes per entry) | 0.70 s |
| Read from JSON and rebuild | 1.3 s |

Compiling a definition: 0.3 ms for 100 states, 3 ms for 1000, 35 ms for 10 000.

Where it is slow or large:

- **Against compiled state machines.** The same five-state cycle in
  [statig](https://crates.io/crates/statig) 0.4, which is compiled from Rust code and keeps no
  record, takes 0.4 ns per event; foldstate takes 210 ns. That is about 500 times slower.
  The cost is looking the event up by name, interpreting the definition, and building the
  reversible change. Use a compiled state machine when the machine is fixed at build time
  and you do not need the log.
- **In WebAssembly.** 200 000 appends to a 100-region machine followed by a fold take 0.26 s
  natively and 1.6 s in Node 22: about 6 times slower.
- **Binary size.** Everything in this crate, including reading and writing every JSON form,
  adds 666 kB to a `wasm32-unknown-unknown` binary (199 kB gzip) at `opt-level = 3`, and
  392 kB (125 kB gzip) at `opt-level = "z"`. A viewer that only follows a serialised log
  with `WireSnapshot::redo` and `undo` adds 213 kB (65 kB gzip), or 155 kB (50 kB gzip) at
  `opt-level = "z"`. Most of it is generated JSON code. Reading numbers exactly adds a
  further 39 kB (12 kB gzip) to a program that did not already ask `serde_json` for it.

## Differences from SCXML and XState

Execution follows the algorithm of W3C SCXML 1.0 (run-to-completion macrosteps, internal
queue first, document-order priority, conflicts between parallel regions). The differences
that matter most in practice:

| | SCXML | XState v5 | foldstate |
|---|---|---|---|
| Event descriptor `a.b` | Matches `a.b` and `a.b.c` | Matches `a.b` only | Matches `a.b` only; write `a.b.*` for the prefix |
| Event that nothing handles | Dropped | Dropped | Rejected with an error, unless the definition ignores it |
| Guards and data | ECMAScript, XPath | JavaScript functions | Comparison, `in`, `+ - *`, and guards named for the host; no scripting |
| A guard that cannot be evaluated | False, plus an error event | Throws | False |
| An action that fails | Error event, rest of the block skipped | Throws | The step fails and changes nothing |
| Domain of a transition | Nearest compound ancestor | Nearest ancestor of any kind | Nearest compound ancestor |
| Ancestor's transition when a parallel region handles the event | Taken for the other regions | Not taken | Taken for the other regions |
| Conflicting transitions | First in document order wins | First entered wins | First in document order wins |
| On completion | All states exited | All states exited | Configuration kept; work stopped, timers cancelled |
| Child machines | `invoke` starts a session | Actors | `invoke` produces start and stop effects; the host runs the child |

Every difference, with the reasons, is in [the semantics](docs/semantics.md).

Conformance: of the 200 tests of the W3C SCXML 1.0 implementation report, 34 use only
constructs foldstate has, and all 34 pass; the other 166 need scripting, event data, `invoke`
or sending to other targets ([list with reasons](tests/w3c/expected.json)). On generated
statecharts restricted to what both express the same way, foldstate and XState 5.33 agree on
9998 of 10 000 cases; the two others are the conflict-order difference above.

## Limits

- No expression language beyond comparison and `+`, `-`, `*`. Put richer logic in a named
  guard or compute it before sending the event.
- A state starts work through `invoke`; connecting two machines (a child with its own log)
  is the host's job.
- SCXML and XState definitions cannot be imported yet.
- Numbers in JSON are read exactly (`serde_json` with `float_roundtrip`), so a stored log
  verifies bit for bit; if you read snapshots or logs with your own JSON reader, it must do
  the same.
- `enabled` looks at each transition on its own; `explain` gives the exact answer for one
  event.
- Definitions from sources you do not control: use `Machine::compile_with` and `Limits`.

## Not in scope

Scripting, I/O, async, threads and GPU computation.

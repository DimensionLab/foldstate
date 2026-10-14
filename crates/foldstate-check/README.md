<!-- SPDX-License-Identifier: Apache-2.0 -->

# foldstate-check

Explores every state a [foldstate](../foldstate) machine can reach and reports what is wrong
with it before it runs: states that are never reached, transitions that are never taken,
deadlocks, steps that fail, broken invariants, and states from which the machine can no
longer finish. Each finding comes with the shortest event sequence that leads to it, and that
sequence replays into a log you can step through.

Experimental. Licence: Apache-2.0. Minimum Rust version: 1.89.

## Use it

```rust
use foldstate::{Definition, Guard, Machine, NoGuards};
use foldstate_check::Explorer;

let definition: Definition = serde_json::from_str(r##"{
  "schema": "foldstate/1", "id": "door",
  "root": {"id": "door", "states": [
    {"id": "closed", "on": [{"event": "open", "target": ["opened"]},
                             {"event": "lock", "target": ["locked"]}]},
    {"id": "opened", "on": [{"event": "close", "target": ["closed"]}]},
    {"id": "locked"},
    {"id": "broken"}
  ]}
}"##)?;
let machine = Machine::compile(&definition)?;

let report = Explorer::new(&machine)
    .never("open while locked", Guard::All(vec![
        Guard::In("#opened".into()),
        Guard::In("#locked".into()),
    ]))
    .run()?;

assert!(report.complete);
assert_eq!(report.unreached_states, ["broken"]);
// Nothing leaves `locked`: a deadlock, reached by one event.
assert_eq!(report.deadlocks.len(), 1);
assert_eq!(report.deadlocks[0].events[0].name, "lock");
assert!(report.violations.is_empty());

// The shortest way to every state, usable as test cases.
assert_eq!(report.paths_to_states["opened"].events.len(), 1);

// A finding replays into a log: step through it with a cursor.
let log = report.deadlocks[0].to_log(&machine, &NoGuards)?;
assert!(machine.is_in(log.head(foldstate::MAIN)?, "locked"));
# Ok::<(), Box<dyn std::error::Error>>(())
```

## What is explored

The search is breadth-first from the start state. In each state it tries every event of the
alphabet and the event of every pending timer.

- **Alphabet.** By default, every event name the definition names exactly, without data. Add
  events with `Explorer::event` when a guard reads the event's data or when only a pattern
  (`step.*`) matches.
- **Host guards.** Pass them with `Explorer::guards`.
- **Bounds.** `Bounds { max_states, max_depth }`; the default stops at 100 000 states. When a
  bound stops the search, `report.complete` is false: findings stand, but an empty report
  proves nothing.

Two states are the same when their snapshots are equal. A context value that grows without
limit, such as a counter, makes the number of states infinite: bound the search, or leave
such data out of the machine you check.

## What is reported

| Field                                     | Meaning                                                                                |
| ----------------------------------------- | -------------------------------------------------------------------------------------- |
| `unreached_states`, `unused_transitions`  | Never active; never taken.                                                             |
| `deadlocks`                               | Not done, and nothing in the alphabet or among the pending timers causes a transition. |
| `failures`                                | A reachable step that fails: a livelock, or an action that cannot be evaluated.        |
| `violations`                              | A reachable state where an invariant (`always`, `never`, `always_fn`) does not hold.   |
| `can_finish`, `cannot_finish_from`        | Whether a done state is reachable, and a state from which it no longer is.             |
| `paths_to_states`, `paths_to_transitions` | The shortest event sequence to each.                                                   |

## With stateright

The `stateright` feature adds `foldstate_check::stateright::Chart`, which presents a machine
as a [stateright](https://crates.io/crates/stateright) model: its breadth-first and
depth-first checkers, `always`, `sometimes` and `eventually` properties, and its explorer.

```toml
foldstate-check = { version = "0.1", features = ["stateright"] }
```

## Limits

- The number of states of a machine with many parallel regions grows exponentially with the
  number of regions. Check one region with its neighbours, or bound the search.
- Properties over time ("every request is eventually answered") are limited to
  `cannot_finish_from`.

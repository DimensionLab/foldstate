<!-- SPDX-License-Identifier: Apache-2.0 -->

# Semantics

This page defines what a definition means: which states are active after an event, in what
order actions run, and what is recorded. The JSON forms are in [`schema/`](schema/).

The execution model is the one of W3C SCXML 1.0 (section 3.13 and Appendix D) with the
differences listed in [Differences from SCXML](#differences-from-scxml).

## Definition

A definition is a tree of states with one root. A state has an `id` unique among its
siblings; its **path** is the ids from the root's child down, joined with `.`. The root's own
`id` is not part of any path, and the root's path is the empty string.

| Kind       | Meaning                                                                                                                                                                                                                                                                                                                                                 |
| ---------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `atomic`   | No child states.                                                                                                                                                                                                                                                                                                                                        |
| `compound` | While active, exactly one child state is active. `initial` names the child entered by default; when absent, the first child.                                                                                                                                                                                                                            |
| `parallel` | While active, every child state is active. Each child is a region.                                                                                                                                                                                                                                                                                      |
| `final`    | A final state of its compound parent. It has no transitions.                                                                                                                                                                                                                                                                                            |
| `history`  | A pseudo-state of a compound or parallel parent. A transition that targets it re-enters what was active the last time the parent was exited. `deep: true` remembers the full active sub-configuration, otherwise the direct children. Before the parent has been exited once, `default` is used, or the parent's default entry when `default` is empty. |

When `kind` is absent a state with children is `compound` and one without is `atomic`. The
root is `compound` or `parallel`.

**Document order** is the order of a depth-first, parent-first walk of the tree.

### State references

Transition targets, history defaults and `in` guards refer to states:

| Form   | Refers to                                                   |
| ------ | ----------------------------------------------------------- |
| `name` | The sibling `name` of the state that holds the reference.   |
| `.a.b` | The descendant `a.b` of the state that holds the reference. |
| `#a.b` | The state with path `a.b`.                                  |

A transition with several targets enters several regions at once: its targets must lie in
different regions of a parallel state, and none may contain another. A history state counts
as its parent here. A transition cannot target the root.

### Events

An event has a `name`, optional `data` and an optional time `at` in milliseconds. A
transition's `event` is a space-separated list of descriptors:

| Descriptor  | Matches                                                                                            |
| ----------- | -------------------------------------------------------------------------------------------------- |
| `step.done` | The event named `step.done`, and no other.                                                         |
| `step.*`    | `step` and every name that continues it after a dot: `step.done`, `step.done.late`. Not `stepper`. |
| `*`         | Every event.                                                                                       |

A transition without `event` is **eventless**: it is taken as soon as its source is active
and its guard holds.

### Guards

| Guard                                          | True when                                                                                                                                                                         |
| ---------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `{"in": ref}`                                  | The state is active.                                                                                                                                                              |
| `{"not": g}`, `{"all": [g…]}`, `{"any": [g…]}` | As named. `all` of nothing is true, `any` of nothing is false.                                                                                                                    |
| `{"cmp": {"left": e, "op": o, "right": e}}`    | `eq`, `ne`: numbers compare by value (`1` equals `1.0`), everything else structurally. `lt`, `le`, `gt`, `ge`: two numbers, two strings or two booleans; false for anything else. |
| `{"named": {"name": n, "params": v}}`          | The host's guard `n` returns true for `(params, context, event)`.                                                                                                                 |

A guard that cannot be evaluated (arithmetic on something that is not a number, a named
guard the host does not have) is **false**. Guards never fail a step, so the order in which
guards are evaluated cannot change the result.

A host guard must be pure. Its results are recorded in the log entry.

### Expressions

`{"lit": v}`, `{"ctx": pointer}`, `{"event": pointer}`, `{"add": [a, b]}`, `{"sub": [a, b]}`,
`{"mul": [a, b]}`. Pointers are JSON Pointers (RFC 6901) into the context or into the data of
the event being processed; a missing value is `null`. Integer arithmetic is checked 64-bit;
if either operand is not an integer the operation is done in IEEE 754 double precision and
the result must be finite.

### Actions

Actions run on exit, on a transition and on entry. They perform no I/O.

| Action                           | Effect                                                                                                                     |
| -------------------------------- | -------------------------------------------------------------------------------------------------------------------------- |
| `assign {path, value}`           | Sets the context value at `path`, creating missing objects. An existing array element can be replaced; arrays do not grow. |
| `raise {event, data?}`           | Appends an event to the internal queue.                                                                                    |
| `emit {name, params?}`           | Produces the effect `emit` for the host.                                                                                   |
| `schedule {id, event, delay_ms}` | Records timer `id` and produces the effect `schedule`. An existing timer with the same id is replaced.                     |
| `cancel {id}`                    | Removes timer `id` and produces the effect `cancel`, when the timer is pending.                                            |

An action that fails (overflow, a write through something that is not an object) fails the
whole step: the snapshot is unchanged and the error names the event.

### Delayed transitions and invocations

`after: [{delay_ms, id?, …transition}]` on a state is shorthand for: on entry, after the
entry actions, `schedule {id, event: "after.<id>", delay_ms}`; on exit, after the exit
actions, `cancel {id}`; and a transition on `after.<id>` placed after the state's `on`
transitions. The default id is `<state path>:after:<index>`.

`invoke: [{id?, name, params?}]` on a state declares work the state owns. The effect `start`
is produced at the end of the macrostep in which the state was entered, if the state is still
active; the effect `stop` is produced when the state is exited, after its exit actions, if
the work was started. The default id is `<state path>:invoke:<index>`. What `name` means is
up to the host: a job, a request, another machine with its own log. The host reports results
as ordinary events.

## Time

The kernel never reads a clock. A timer is the host's promise to deliver the timer's event
later. When an event with the name of a pending timer arrives, the timer is no longer
pending. `due_ms` is the triggering event's `at` plus the delay, when `at` is present.

## Processing an event

A **snapshot** is the state between two events: the active states (the configuration), the
history, the context, the pending timers, the started invocations and the status.

Processing one external event is one **macrostep**:

1. If the status is `done`, the event is refused (`Done`).
2. Timers whose event has this name stop being pending.
3. **Select** the transitions the event enables. An event named `done.state.…`, or the
   event of an `after` timer that is not pending, enables none: the kernel produces these
   itself and does not accept them from outside. If there are none: the event is **ignored**
   when the definition's `unhandled` is `ignore` or a descriptor in `ignore` matches it, and
   **rejected** otherwise. A rejected event changes nothing, not even the timers.
4. Take the selected transitions (a **microstep**), then repeat: select eventless
   transitions and take them; when there are none, take the next event from the internal
   queue, select and take its transitions; stop when no eventless transition is enabled and
   the queue is empty. An internal event that enables nothing is dropped.
5. Produce `start` for the invocations of states entered during the macrostep that are still
   active, in document order.

More than `max_microsteps` microsteps (default 1000) in one macrostep is a `Livelock` error.

Starting a machine is the same from step 4 on, with "enter the root's default configuration"
as the first microstep. The context starts as the definition's `context`, with the keys of
the start input (an object) written over it. During start, the event seen by expressions is
`foldstate.init` with the input as data.

### Selecting transitions

For each active atomic state in document order: look at the state, then its parent, and so
on up to the root; in each, look at the transitions in order; the first one whose descriptor
matches (or that is eventless, when selecting eventless transitions) and whose guard holds is
chosen for that atomic state. A transition chosen by several atomic states counts once.

Chosen transitions can conflict: two transitions conflict when the sets of states they would
exit intersect. Going through the chosen transitions in order, a transition is dropped when
it conflicts with one already kept, unless its source is a descendant of that one's source,
in which case it replaces it.

### Taking transitions (microstep)

The **domain** of a transition with targets is the state inside which it happens:

- its source, when the transition is `internal`, the source is compound and every target is
  a descendant of the source;
- the root, when the source is the root. The root itself is never exited or entered; when it
  is parallel, every region the transition does not target is entered by default;
- otherwise the nearest proper ancestor of the source that is compound (or the root) and
  contains every target.

A history target stands for the states it would restore.

1. **Exit.** The exit set is every active proper descendant of the domain of every taken
   transition. For each state in the exit set that has history children, the history is
   recorded. Then, in reverse document order, each state runs its exit actions, cancels its
   `after` timers, stops its invocations and becomes inactive.
2. **Transition actions**, in the order of the taken transitions.
3. **Enter.** The entry set is the targets, their ancestors below the domain, and the
   default descendants of every entered compound or parallel state that is not otherwise
   specified; a region that was not exited is left as it is. In document order, each state
   that is not already active becomes active, runs its entry actions and schedules its
   `after` timers.

A transition without targets runs only step 2.

### Final states and completion

When a final state is entered:

- if its parent is the root, the machine is **done**;
- otherwise `done.state.<path of the parent>` is raised; and if the parent's parent is a
  parallel state all of whose regions are in a final state, `done.state.<path of that state>`
  is raised, or, if that state is the root, the machine is done.

A compound state is in a final state when its active child is final; a parallel state when
all its regions are.

When the machine is done, the macrostep ends after the current microstep: the queue is
dropped, no eventless transition is taken, started invocations get `stop` and pending timers
get `cancel` (in order of id), and the status becomes `done`. The configuration is kept as
it is, and exit actions do not run.

## Determinism

For one definition, the same snapshot and the same event give the same snapshot, the same
effects in the same order and the same log entry, on every platform. The kernel uses no
clock, no randomness, no hash-ordered collection and no floating-point operation other than
`+`, `-`, `*` and comparison on finite doubles.

## Log

A log is a tree of **branches**. A branch is a list of **entries** with consecutive sequence
numbers starting at 1. Nothing in a log is ever changed or removed.

An entry records:

- what happened: `start` (with the input), `event` (with the event) or `restore` (with the
  sequence number restored to);
- `handled`: false when the event was ignored;
- `ops`: the reversible changes to the snapshot, in order (`enter`, `exit`, `set`, `history`,
  `timer`, `invocation`, `status`);
- `effects`, in order;
- `microsteps`: the transitions taken, as `(source path, index)`;
- `guards`: the results of host guards.

A rejected event is not recorded.

**State at N** is the result of applying the `ops` of entries 1 to N. Applying an entry's ops
needs neither guards nor the host; applying their inverses in reverse order goes back.

**Verify** recomputes every entry from the definition and the recorded events and compares
ops, effects and outcome. A difference is reported with its branch and sequence number.

**Fork** at N creates a branch whose entries 1 to N are those of its parent and whose next
entry is N + 1. The parent is unaffected.

**Restore** to N appends an entry whose ops turn the current snapshot into the snapshot at N
on the same branch. Its effects reconcile the host: `cancel` for timers that are no longer
pending, `stop` for invocations no longer started, then `schedule` and `start` for those that
are again (in order of id). Exit, entry and transition actions do not run.

A log names the SHA-256 of the canonical JSON (RFC 8785) of its definition. A machine
compiled from a different definition refuses it.

## Formats and versions

Three serialised forms, each naming its version:

| Form       | `schema`               | Covers                                                                               |
| ---------- | ---------------------- | ------------------------------------------------------------------------------------ |
| Definition | `foldstate/1`          | States, transitions, guards, expressions, actions.                                   |
| Snapshot   | `foldstate-snapshot/1` | Configuration, history, context, timers, invocations, status.                        |
| Log        | `foldstate-log/1`      | Branches and entries: what happened, ops, effects, transitions taken, guard results. |

A number changes when a document written under the old one would be read differently or
refused. Fields may be added within a version only when their absence means what it meant
before. A reader refuses a `schema` it does not know. The exact bytes of each form for a
fixed example are kept in `tests/golden/` and compared on every test run.

## Differences from SCXML

| SCXML                                                                                                                                             | Here                                                                                                                                                       |
| ------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- |
| ECMAScript, XPath or null data model; `<script>`                                                                                                  | The guard, expression and action forms above.                                                                                                              |
| A condition that fails raises `error.execution`                                                                                                   | The guard is false.                                                                                                                                        |
| Executable content that fails raises `error.execution` and the rest of the block is skipped                                                       | The step fails and changes nothing.                                                                                                                        |
| A descriptor matches every event name it is a prefix of (`error` matches `error.send`)                                                            | A descriptor matches its own name only; prefix matching is written `error.*`. State paths contain dots, so `done.state.a` must not match `done.state.a.b`. |
| Any event can be sent from outside, including `done.state.*`                                                                                      | `done.state.*` and the events of `after` timers that are not pending are not accepted from outside.                                                        |
| An event that enables nothing is dropped                                                                                                          | External events are rejected unless ignored by the definition; internal events are dropped.                                                                |
| `<scxml>` has no transitions and cannot be parallel                                                                                               | The root can hold transitions (its domain is the root) and can be parallel.                                                                                |
| On completion all states are exited and their `onexit` runs                                                                                       | The configuration is kept; invocations are stopped and timers cancelled.                                                                                   |
| `<invoke>` starts a session and exchanges events with it                                                                                          | `start` and `stop` effects; the host connects the two machines.                                                                                            |
| `<send>` with delay, targets and I/O processors                                                                                                   | `schedule` and `cancel` with timers in the snapshot; `emit` for everything else.                                                                           |
| `<initial>` and history defaults can hold executable content; `initial` can name deep descendants                                                 | `initial` names a child; neither holds actions.                                                                                                            |
| `done.state` events carry `<donedata>`                                                                                                            | No data.                                                                                                                                                   |
| A state in the entry set that was not exited (reachable through a history state of an ancestor, targeted from inside it) runs its `onentry` again | A state that is already active is not entered again.                                                                                                       |
| `<final>` under `<scxml>` only                                                                                                                    | Also: a parallel root is done when all its regions are in a final state.                                                                                   |
| Invocations of a state are started before eventless processing continues if the queue is non-empty                                                | Started once, at the end of the macrostep.                                                                                                                 |

## Differences from XState v5

Found by running generated statecharts through both (`tests/xstate.rs`). Where SCXML and
XState differ, foldstate follows SCXML.

| XState v5                                                                                        | Here                                                                                                                                                                                                                 |
| ------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| The domain of a transition is the nearest common ancestor of its source and targets, of any kind | The nearest one that is a compound state (or the root), as in SCXML: a transition out of a region to a state outside it exits and re-enters the whole parallel state, and everything else below that compound state. |
| In a parallel state, a transition of an ancestor is not taken once any region handles the event  | Every active atomic state looks for a transition on its own, as in SCXML: the ancestor's transition is taken for the regions that have none, unless it conflicts with one already chosen.                            |
| `reenter: false` on a transition whose target is its own atomic source skips exit and entry      | `internal` has an effect only when the source is compound and every target is a proper descendant of it, as in SCXML.                                                                                                |
| When the machine finishes, every state is exited and exit actions run                            | The configuration is kept and no exit action runs.                                                                                                                                                                   |
| An event that enables nothing is dropped                                                         | External events are rejected unless the definition ignores them.                                                                                                                                                     |
| A transition on the root can re-enter the root                                                   | The root is never exited or entered.                                                                                                                                                                                 |
| Completion events are named `xstate.done.state.<id>`                                             | `done.state.<path>`.                                                                                                                                                                                                 |

With these left out of the generated machines, 9998 of 10 000 cases (253 872 events) give
the same active states and context after every event. The two that differ are instances of
one more difference:

| XState v5                                                                                   | Here                                                                                                                                                                                           |
| ------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| When two enabled transitions conflict, the one whose atomic state was entered first is kept | The one whose atomic state comes first in the document is kept, as in SCXML. The results differ once a state has been exited and entered again, because it then comes later in XState's order. |

The smallest machine that shows it is in `tests/xstate.rs`.

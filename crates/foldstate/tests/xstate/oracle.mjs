// SPDX-License-Identifier: Apache-2.0
// Runs statecharts written for foldstate in XState v5 and compares the active states and
// the counter after every event with what foldstate computed. Input: a JSON file written
// by tests/xstate.rs. Output: one JSON line per case that differs, then a summary line.
import { readFileSync } from 'node:fs';
import {
  and,
  assign,
  createActor,
  createMachine,
  not,
  or,
  raise,
  stateIn,
} from 'xstate';

const id = (path) => 's_' + path.replaceAll('.', '__');
const absolute = (ref) => ref.slice(1); // the generator writes `#path` references only

// XState has no bound on the steps one event may cause; this stops a run that does not end.
let fuel = 0;
const burn = () => {
  if (--fuel < 0) throw new Error('does not settle');
};

function guard(g) {
  const inner = guardOf(g);
  // XState's own guards (stateIn, and, or, not) are recognised by XState, not called.
  if (typeof inner !== 'function' || inner.check) return inner;
  return (args) => {
    burn();
    return inner(args);
  };
}

function guardOf(g) {
  if ('in' in g) return stateIn('#' + id(absolute(g.in)));
  if ('not' in g) return not(guard(g.not));
  if ('all' in g) return and(g.all.map(guard));
  if ('any' in g) return or(g.any.map(guard));
  const { op, right } = g.cmp;
  const k = right.lit;
  const test = {
    eq: (n) => n === k,
    ne: (n) => n !== k,
    lt: (n) => n < k,
    le: (n) => n <= k,
    gt: (n) => n > k,
    ge: (n) => n >= k,
  }[op];
  return ({ context }) => test(context.n);
}

function action(a) {
  if ('assign' in a)
    return assign({
      n: ({ context }) => {
        burn();
        return context.n + 1;
      },
    });
  if ('raise' in a) return [burn, raise({ type: a.raise.event })];
  return burn; // emit: effects are not compared
}

function kindOf(s) {
  return s.kind ?? (s.states?.length ? 'compound' : 'atomic');
}

function state(s, path, root) {
  const node = {};
  if (!root) node.id = id(path);
  const kind = kindOf(s);
  if (kind === 'history') {
    node.type = 'history';
    node.history = s.deep ? 'deep' : 'shallow';
    if (s.default?.length) node.target = '#' + id(absolute(s.default[0]));
    return node;
  }
  if (kind === 'parallel') node.type = 'parallel';
  if (kind === 'final') node.type = 'final';
  if (s.states?.length) {
    node.states = {};
    for (const c of s.states)
      node.states[c.id] = state(c, root ? c.id : `${path}.${c.id}`, false);
  }
  if (kind === 'compound') {
    node.initial =
      s.initial ?? s.states.find((c) => kindOf(c) !== 'history').id;
  }
  const always = [];
  const on = {};
  for (const t of s.on ?? []) {
    const tr = {};
    if (t.target?.length) {
      tr.target = t.target.map((r) => '#' + id(absolute(r)));
      // SCXML: a transition counts as internal when its source is compound and every target
      // is a proper descendant of it; otherwise `internal` has no effect. XState would also
      // skip the exit and entry of an atomic state that targets itself.
      const inside = t.target.every((r) => absolute(r).startsWith(path + '.'));
      tr.reenter = !(t.internal && kind === 'compound' && inside);
    }
    if (t.guard) tr.guard = guard(t.guard);
    if (t.actions?.length) tr.actions = t.actions.flatMap(action);
    if (t.event === undefined) always.push(tr);
    else (on[t.event] ??= []).push(tr);
  }
  if (always.length) node.always = always;
  if (Object.keys(on).length) node.on = on;
  if (s.entry?.length) node.entry = s.entry.flatMap(action);
  if (s.exit?.length) node.exit = s.exit.flatMap(action);
  return node;
}

function paths(value, prefix = '') {
  if (typeof value === 'string') return [prefix + value];
  const out = [];
  for (const [k, v] of Object.entries(value)) {
    out.push(prefix + k);
    out.push(...paths(v, `${prefix}${k}.`));
  }
  return out;
}

function observe(actor) {
  const s = actor.getSnapshot();
  return {
    config: paths(s.value).sort(),
    n: s.context.n,
    done: s.status === 'done',
  };
}

// When a machine finishes, XState exits every state and runs the exit actions; foldstate
// keeps the configuration and runs none. The counter is therefore not compared at the end.
const same = (a, b) =>
  a.done === b.done &&
  (a.done || a.n === b.n) &&
  JSON.stringify(a.config) === JSON.stringify(b.config);

// XState reports an error thrown inside a step a second time, asynchronously.
process.on('uncaughtException', () => {});

const { cases } = JSON.parse(readFileSync(process.argv[2], 'utf8'));
let differing = 0;
let events = 0;
for (const c of cases) {
  let actor;
  fuel = 100000;
  try {
    const machine = createMachine({
      id: 'm',
      context: { n: 0 },
      ...state(c.definition.root, '', true),
    });
    actor = createActor(machine);
    actor.start();
  } catch (error) {
    differing++;
    console.log(
      JSON.stringify({ seed: c.seed, at: 'start', error: String(error) }),
    );
    continue;
  }
  let got = observe(actor);
  if (!same(got, c.start)) {
    differing++;
    console.log(
      JSON.stringify({
        seed: c.seed,
        at: 'start',
        xstate: got,
        foldstate: c.start,
      }),
    );
    continue;
  }
  for (let i = 0; i < c.events.length; i++) {
    if (got.done) break;
    events++;
    fuel = 100000;
    try {
      actor.send({ type: c.events[i] });
    } catch (error) {
      differing++;
      console.log(
        JSON.stringify({
          seed: c.seed,
          at: i,
          event: c.events[i],
          error: String(error),
        }),
      );
      break;
    }
    if (actor.getSnapshot().status === 'error') {
      differing++;
      console.log(
        JSON.stringify({
          seed: c.seed,
          at: i,
          event: c.events[i],
          error: 'does not settle',
        }),
      );
      break;
    }
    got = observe(actor);
    if (!same(got, c.expected[i])) {
      differing++;
      console.log(
        JSON.stringify({
          seed: c.seed,
          at: i,
          event: c.events[i],
          xstate: got,
          foldstate: c.expected[i],
        }),
      );
      break;
    }
  }
}
console.log(JSON.stringify({ cases: cases.length, events, differing }));

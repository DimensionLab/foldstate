// SPDX-License-Identifier: Apache-2.0
//! Reference model: a direct, unoptimised transcription of the interpretation algorithm of
//! W3C SCXML 1.0, Appendix D, restricted and extended as `docs/semantics.md` describes.
//!
//! It shares only the data types with the crate (definition, event, effect, serialised
//! snapshot). Sets are ordered sets of state numbers in document order, ancestors are found
//! by walking parents, every selection scans every active state, and a step works on a copy
//! of the state. Names follow the pseudo-code of the specification so the two can be read
//! side by side.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use foldstate::{
    Action, CmpOp, Definition, Effect, Event, Expr, Guard, Invocation, Kind, StateDef, Status,
    Timer, Unhandled, Value, WireSnapshot,
};
use serde_json::{Map, Number};

pub type GuardFn<'a> = &'a dyn Fn(&str, &Value, &Value, &Event) -> Option<bool>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefError {
    Rejected,
    Done,
    Livelock,
    Eval,
}

struct Node<'a> {
    path: String,
    parent: Option<usize>,
    kind: Kind,
    def: &'a StateDef,
    children: Vec<usize>,
    transitions: Vec<usize>,
    initial: Option<usize>,
    /// History states: default targets.
    default: Vec<usize>,
    /// (timer id, event name, delay)
    after: Vec<(String, String, u64)>,
    /// (invocation id, name, params)
    invoke: Vec<(String, String, Option<Expr>)>,
}

struct Tr {
    source: usize,
    /// `None`: eventless.
    descriptors: Option<Vec<String>>,
    guard: Option<Guard>,
    targets: Vec<usize>,
    internal: bool,
    actions: Vec<Action>,
}

pub struct Model<'a> {
    def: &'a Definition,
    nodes: Vec<Node<'a>>,
    trans: Vec<Tr>,
}

fn join(parent: &str, id: &str) -> String {
    if parent.is_empty() {
        id.to_string()
    } else {
        format!("{parent}.{id}")
    }
}

impl<'a> Model<'a> {
    /// Builds the model. Panics on a definition the crate would refuse: the reference is
    /// only ever given definitions that compiled.
    pub fn new(def: &'a Definition) -> Self {
        let mut m = Model {
            def,
            nodes: Vec::new(),
            trans: Vec::new(),
        };
        m.add(&def.root, None, String::new());
        for i in 0..m.nodes.len() {
            let d = m.nodes[i].def;
            if m.nodes[i].kind == Kind::Compound {
                let want = d.initial.clone().unwrap_or_else(|| {
                    let first = m.nodes[i]
                        .children
                        .iter()
                        .find(|&&c| m.nodes[c].kind != Kind::History)
                        .expect("compound state with a child state");
                    m.nodes[*first].def.id.clone()
                });
                m.nodes[i].initial = Some(m.resolve(i, &format!(".{want}")));
            }
            if m.nodes[i].kind == Kind::History {
                let targets = d.default.iter().map(|t| m.resolve(i, t)).collect();
                m.nodes[i].default = targets;
            }
            let mut list = Vec::new();
            for t in &d.on {
                list.push(Tr {
                    source: i,
                    descriptors: t
                        .event
                        .as_ref()
                        .map(|e| e.split_whitespace().map(str::to_string).collect()),
                    guard: t.guard.clone(),
                    targets: t.target.iter().map(|x| m.resolve(i, x)).collect(),
                    internal: t.internal,
                    actions: t.actions.clone(),
                });
            }
            for (n, a) in d.after.iter().enumerate() {
                let id =
                    a.id.clone()
                        .unwrap_or_else(|| format!("{}:after:{n}", m.nodes[i].path));
                let event = format!("after.{id}");
                list.push(Tr {
                    source: i,
                    descriptors: Some(vec![event.clone()]),
                    guard: a.guard.clone(),
                    targets: a.target.iter().map(|x| m.resolve(i, x)).collect(),
                    internal: a.internal,
                    actions: a.actions.clone(),
                });
                m.nodes[i].after.push((id, event, a.delay_ms));
            }
            for (n, v) in d.invoke.iter().enumerate() {
                let id =
                    v.id.clone()
                        .unwrap_or_else(|| format!("{}:invoke:{n}", m.nodes[i].path));
                m.nodes[i]
                    .invoke
                    .push((id, v.name.clone(), v.params.clone()));
            }
            for t in list {
                m.nodes[i].transitions.push(m.trans.len());
                m.trans.push(t);
            }
        }
        m
    }

    fn add(&mut self, d: &'a StateDef, parent: Option<usize>, path: String) -> usize {
        let i = self.nodes.len();
        self.nodes.push(Node {
            path: path.clone(),
            parent,
            kind: d.resolved_kind(),
            def: d,
            children: Vec::new(),
            transitions: Vec::new(),
            initial: None,
            default: Vec::new(),
            after: Vec::new(),
            invoke: Vec::new(),
        });
        for c in &d.states {
            let child_path = if parent.is_none() {
                c.id.clone()
            } else {
                join(&path, &c.id)
            };
            let ci = self.add(c, Some(i), child_path);
            self.nodes[i].children.push(ci);
        }
        i
    }

    fn find(&self, path: &str) -> usize {
        self.nodes
            .iter()
            .position(|n| {
                n.path == path && n.parent.is_some() || path.is_empty() && n.parent.is_none()
            })
            .unwrap_or_else(|| panic!("no state `{path}`"))
    }

    /// `name`: sibling of `from`; `.a.b`: descendant of `from`; `#a.b`: from the root.
    fn resolve(&self, from: usize, reference: &str) -> usize {
        if let Some(abs) = reference.strip_prefix('#') {
            self.find(abs)
        } else if let Some(rel) = reference.strip_prefix('.') {
            if self.nodes[from].parent.is_none() {
                self.find(rel)
            } else {
                self.find(&join(&self.nodes[from].path, rel))
            }
        } else {
            let parent = self.nodes[from].parent.expect("the root has no siblings");
            if self.nodes[parent].parent.is_none() {
                self.find(reference)
            } else {
                self.find(&join(&self.nodes[parent].path, reference))
            }
        }
    }

    // ---- structure, as in the specification -------------------------------------------

    /// Proper ancestors of `s`, nearest first, up to but not including `upto`.
    fn get_proper_ancestors(&self, s: usize, upto: Option<usize>) -> Vec<usize> {
        let mut out = Vec::new();
        let mut cur = self.nodes[s].parent;
        while let Some(p) = cur {
            if Some(p) == upto {
                break;
            }
            out.push(p);
            cur = self.nodes[p].parent;
        }
        out
    }

    fn is_descendant(&self, s: usize, of: usize) -> bool {
        self.get_proper_ancestors(s, None).contains(&of)
    }

    fn get_child_states(&self, s: usize) -> Vec<usize> {
        self.nodes[s]
            .children
            .iter()
            .copied()
            .filter(|&c| self.nodes[c].kind != Kind::History)
            .collect()
    }

    fn is_atomic(&self, s: usize) -> bool {
        matches!(self.nodes[s].kind, Kind::Atomic | Kind::Final)
    }

    fn is_root(&self, s: usize) -> bool {
        self.nodes[s].parent.is_none()
    }

    fn find_lcca(&self, states: &[usize]) -> Option<usize> {
        self.get_proper_ancestors(states[0], None)
            .into_iter()
            .filter(|&a| self.nodes[a].kind == Kind::Compound || self.is_root(a))
            .find(|&a| states[1..].iter().all(|&s| self.is_descendant(s, a)))
    }
}

/// `*` matches everything; `a.*` matches `a` and whatever continues it after a dot; anything
/// else matches only itself.
fn name_match(descriptors: &[String], name: &str) -> bool {
    descriptors.iter().any(|d| {
        d == "*"
            || d == name
            || d.strip_suffix(".*").is_some_and(|p| {
                name == p || (name.starts_with(p) && name[p.len()..].starts_with('.'))
            })
    })
}

/// The interpreter state: the datamodel of Appendix D.
#[derive(Clone)]
struct St {
    configuration: BTreeSet<usize>,
    history_value: BTreeMap<usize, Vec<usize>>,
    context: Value,
    timers: BTreeMap<String, Timer>,
    invocations: BTreeMap<String, Invocation>,
    running: bool,
    internal_queue: VecDeque<Event>,
    states_to_invoke: BTreeSet<usize>,
    /// When each active state was entered: a running count. Only used to imitate XState.
    entered: BTreeMap<usize, u64>,
    clock: u64,
    effects: Vec<Effect>,
    /// `_event`: the event being processed.
    event: Event,
    /// Time of the external event.
    now: Option<i64>,
}

pub struct Interpreter<'a, 'g> {
    m: &'a Model<'a>,
    guards: GuardFn<'g>,
    machine: String,
    entry_order: bool,
}

impl<'a, 'g> Interpreter<'a, 'g> {
    pub fn new(model: &'a Model<'a>, machine_hash: &str, guards: GuardFn<'g>) -> Self {
        Interpreter {
            m: model,
            guards,
            machine: machine_hash.to_string(),
            entry_order: false,
        }
    }

    /// Not SCXML: looks at the active atomic states in the order they were entered instead
    /// of in document order, as XState v5 does. Only the order within one macrostep is
    /// imitated. Used to show where the two libraries differ.
    pub fn in_entry_order(mut self) -> Self {
        self.entry_order = true;
        self
    }

    // ---- expressions, guards, actions --------------------------------------------------

    fn eval(&self, st: &St, e: &Expr) -> Result<Value, RefError> {
        Ok(match e {
            Expr::Lit(v) => v.clone(),
            Expr::Ctx(p) => st.context.pointer(p).cloned().unwrap_or(Value::Null),
            Expr::Event(p) => st.event.data.pointer(p).cloned().unwrap_or(Value::Null),
            Expr::Add(a, b) => arith('+', &self.eval(st, a)?, &self.eval(st, b)?)?,
            Expr::Sub(a, b) => arith('-', &self.eval(st, a)?, &self.eval(st, b)?)?,
            Expr::Mul(a, b) => arith('*', &self.eval(st, a)?, &self.eval(st, b)?)?,
        })
    }

    /// A guard that cannot be evaluated is false.
    fn condition_match(&self, st: &St, t: &Tr) -> bool {
        match &t.guard {
            None => true,
            Some(g) => self.guard(st, t.source, g).unwrap_or(false),
        }
    }

    fn guard(&self, st: &St, from: usize, g: &Guard) -> Result<bool, RefError> {
        Ok(match g {
            Guard::In(s) => st.configuration.contains(&self.m.resolve(from, s)),
            Guard::Not(g) => !self.guard(st, from, g)?,
            Guard::All(gs) => {
                for g in gs {
                    if !self.guard(st, from, g)? {
                        return Ok(false);
                    }
                }
                true
            }
            Guard::Any(gs) => {
                for g in gs {
                    if self.guard(st, from, g)? {
                        return Ok(true);
                    }
                }
                false
            }
            Guard::Cmp { left, op, right } => {
                let (l, r) = (self.eval(st, left)?, self.eval(st, right)?);
                match op {
                    CmpOp::Eq => same(&l, &r),
                    CmpOp::Ne => !same(&l, &r),
                    CmpOp::Lt => order(&l, &r).is_some_and(|o| o.is_lt()),
                    CmpOp::Le => order(&l, &r).is_some_and(|o| o.is_le()),
                    CmpOp::Gt => order(&l, &r).is_some_and(|o| o.is_gt()),
                    CmpOp::Ge => order(&l, &r).is_some_and(|o| o.is_ge()),
                }
            }
            Guard::Named { name, params } => {
                (self.guards)(name, params, &st.context, &st.event).ok_or(RefError::Eval)?
            }
        })
    }

    fn execute_content(&self, st: &mut St, actions: &[Action]) -> Result<(), RefError> {
        for a in actions {
            match a {
                Action::Assign { path, value } => {
                    let v = self.eval(st, value)?;
                    assign(&mut st.context, path, v)?;
                }
                Action::Raise { event, data } => {
                    let data = match data {
                        Some(e) => self.eval(st, e)?,
                        None => Value::Null,
                    };
                    st.internal_queue.push_back(Event {
                        name: event.clone(),
                        data,
                        at: st.now,
                    });
                }
                Action::Emit { name, params } => {
                    let params = match params {
                        Some(e) => self.eval(st, e)?,
                        None => Value::Null,
                    };
                    st.effects.push(Effect::Emit {
                        name: name.clone(),
                        params,
                    });
                }
                Action::Schedule {
                    id,
                    event,
                    delay_ms,
                } => self.schedule(st, id, event, *delay_ms),
                Action::Cancel { id } => self.cancel(st, id),
            }
        }
        Ok(())
    }

    fn schedule(&self, st: &mut St, id: &str, event: &str, delay_ms: u64) {
        let due_ms = st
            .now
            .map(|n| n.saturating_add(i64::try_from(delay_ms).unwrap_or(i64::MAX)));
        st.timers.insert(
            id.to_string(),
            Timer {
                event: event.to_string(),
                delay_ms,
                due_ms,
            },
        );
        st.effects.push(Effect::Schedule {
            id: id.to_string(),
            event: event.to_string(),
            delay_ms,
            due_ms,
        });
    }

    fn cancel(&self, st: &mut St, id: &str) {
        if st.timers.remove(id).is_some() {
            st.effects.push(Effect::Cancel { id: id.to_string() });
        }
    }

    // ---- Appendix D ---------------------------------------------------------------------

    fn get_effective_target_states(&self, st: &St, t: &Tr) -> Vec<usize> {
        let mut targets = Vec::new();
        for &s in &t.targets {
            self.effective(st, s, &mut targets);
        }
        targets
    }

    fn effective(&self, st: &St, s: usize, out: &mut Vec<usize>) {
        if self.m.nodes[s].kind == Kind::History {
            match st.history_value.get(&s) {
                Some(v) => {
                    for &x in v {
                        if !out.contains(&x) {
                            out.push(x);
                        }
                    }
                }
                None => {
                    for x in self.history_default(s) {
                        self.effective(st, x, out);
                    }
                }
            }
        } else if !out.contains(&s) {
            out.push(s);
        }
    }

    /// The default of a history state: its `default` targets, or its parent's default entry.
    fn history_default(&self, h: usize) -> Vec<usize> {
        let n = &self.m.nodes[h];
        if !n.default.is_empty() {
            return n.default.clone();
        }
        let parent = n.parent.expect("history has a parent");
        match self.m.nodes[parent].kind {
            Kind::Compound => vec![self.m.nodes[parent].initial.expect("initial")],
            _ => self.m.get_child_states(parent),
        }
    }

    fn get_transition_domain(&self, st: &St, t: &Tr) -> Option<usize> {
        let tstates = self.get_effective_target_states(st, t);
        if tstates.is_empty() {
            return None;
        }
        if t.internal
            && self.m.nodes[t.source].kind == Kind::Compound
            && tstates.iter().all(|&s| self.m.is_descendant(s, t.source))
        {
            return Some(t.source);
        }
        if self.m.is_root(t.source) {
            // A transition on the root never exits the root.
            return Some(t.source);
        }
        let mut all = vec![t.source];
        all.extend(tstates);
        self.m.find_lcca(&all)
    }

    fn compute_exit_set(&self, st: &St, transitions: &[usize]) -> BTreeSet<usize> {
        let mut states_to_exit = BTreeSet::new();
        for &ti in transitions {
            let t = &self.m.trans[ti];
            if t.targets.is_empty() {
                continue;
            }
            if let Some(domain) = self.get_transition_domain(st, t) {
                for &s in &st.configuration {
                    if self.m.is_descendant(s, domain) {
                        states_to_exit.insert(s);
                    }
                }
            }
        }
        states_to_exit
    }

    /// `selectTransitions(event)` and `selectEventlessTransitions()`.
    fn select_transitions(&self, st: &St, event: Option<&str>) -> Vec<usize> {
        let mut enabled: Vec<usize> = Vec::new();
        let atomic_states: Vec<usize> = st
            .configuration
            .iter()
            .copied()
            .filter(|&s| self.m.is_atomic(s))
            .collect();
        let mut atomic_states = atomic_states;
        if self.entry_order {
            atomic_states.sort_by_key(|s| st.entered.get(s).copied().unwrap_or(0));
        }
        for state in atomic_states {
            let mut chain = vec![state];
            chain.extend(self.m.get_proper_ancestors(state, None));
            'state: for s in chain {
                for &ti in &self.m.nodes[s].transitions {
                    let t = &self.m.trans[ti];
                    let matches = match (event, &t.descriptors) {
                        (None, None) => true,
                        (Some(name), Some(d)) => name_match(d, name),
                        _ => false,
                    };
                    if matches && self.condition_match(st, t) {
                        if !enabled.contains(&ti) {
                            enabled.push(ti);
                        }
                        break 'state;
                    }
                }
            }
        }
        self.remove_conflicting_transitions(st, enabled)
    }

    fn remove_conflicting_transitions(&self, st: &St, enabled: Vec<usize>) -> Vec<usize> {
        let mut filtered: Vec<usize> = Vec::new();
        for t1 in enabled {
            let mut t1_preempted = false;
            let mut to_remove = Vec::new();
            for &t2 in &filtered {
                let e1 = self.compute_exit_set(st, &[t1]);
                let e2 = self.compute_exit_set(st, &[t2]);
                if e1.intersection(&e2).next().is_some() {
                    if self
                        .m
                        .is_descendant(self.m.trans[t1].source, self.m.trans[t2].source)
                    {
                        to_remove.push(t2);
                    } else {
                        t1_preempted = true;
                        break;
                    }
                }
            }
            if !t1_preempted {
                filtered.retain(|t| !to_remove.contains(t));
                filtered.push(t1);
            }
        }
        filtered
    }

    fn microstep(&self, st: &mut St, enabled: &[usize]) -> Result<(), RefError> {
        self.exit_states(st, enabled)?;
        for &ti in enabled {
            self.execute_content(st, &self.m.trans[ti].actions)?;
        }
        self.enter_states(st, enabled)
    }

    fn exit_states(&self, st: &mut St, enabled: &[usize]) -> Result<(), RefError> {
        let states_to_exit = self.compute_exit_set(st, enabled);
        for &s in &states_to_exit {
            st.states_to_invoke.remove(&s);
        }
        for &s in &states_to_exit {
            for &h in &self.m.nodes[s].children {
                if self.m.nodes[h].kind != Kind::History {
                    continue;
                }
                let value: Vec<usize> = st
                    .configuration
                    .iter()
                    .copied()
                    .filter(|&s0| {
                        if self.m.nodes[h].def.deep {
                            self.m.is_atomic(s0) && self.m.is_descendant(s0, s)
                        } else {
                            self.m.nodes[s0].parent == Some(s)
                        }
                    })
                    .collect();
                st.history_value.insert(h, value);
            }
        }
        // Exit order: reverse document order.
        for &s in states_to_exit.iter().rev() {
            self.execute_content(st, &self.m.nodes[s].def.exit)?;
            for (id, _, _) in &self.m.nodes[s].after {
                self.cancel(st, id);
            }
            for (id, _, _) in &self.m.nodes[s].invoke {
                if st.invocations.remove(id).is_some() {
                    st.effects.push(Effect::Stop { id: id.clone() });
                }
            }
            st.configuration.remove(&s);
        }
        Ok(())
    }

    fn enter_states(&self, st: &mut St, enabled: &[usize]) -> Result<(), RefError> {
        let mut states_to_enter = BTreeSet::new();
        for &ti in enabled {
            let t = &self.m.trans[ti];
            for &s in &t.targets {
                self.add_descendant_states_to_enter(st, s, &mut states_to_enter);
            }
            let ancestor = self.get_transition_domain(st, t);
            for s in self.get_effective_target_states(st, t) {
                self.add_ancestor_states_to_enter(st, s, ancestor, &mut states_to_enter);
            }
        }
        self.enter(st, &states_to_enter)
    }

    /// Enters the given states in document order.
    fn enter(&self, st: &mut St, states_to_enter: &BTreeSet<usize>) -> Result<(), RefError> {
        for &s in states_to_enter {
            if st.configuration.contains(&s) {
                // Not in Appendix D, which would run the entry actions of a state that was
                // never exited. See "Differences from SCXML".
                continue;
            }
            st.configuration.insert(s);
            st.clock += 1;
            st.entered.insert(s, st.clock);
            st.states_to_invoke.insert(s);
            self.execute_content(st, &self.m.nodes[s].def.entry)?;
            for (id, event, delay) in &self.m.nodes[s].after {
                self.schedule(st, id, event, *delay);
            }
            if self.m.nodes[s].kind == Kind::Final {
                let parent = self.m.nodes[s].parent.expect("final has a parent");
                if self.m.is_root(parent) {
                    st.running = false;
                } else {
                    st.internal_queue.push_back(Event {
                        name: format!("done.state.{}", self.m.nodes[parent].path),
                        data: Value::Null,
                        at: st.now,
                    });
                    let grandparent = self.m.nodes[parent].parent.expect("not the root");
                    if self.m.nodes[grandparent].kind == Kind::Parallel
                        && self
                            .m
                            .get_child_states(grandparent)
                            .iter()
                            .all(|&c| self.is_in_final_state(st, c))
                    {
                        if self.m.is_root(grandparent) {
                            st.running = false;
                        } else {
                            st.internal_queue.push_back(Event {
                                name: format!("done.state.{}", self.m.nodes[grandparent].path),
                                data: Value::Null,
                                at: st.now,
                            });
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn is_in_final_state(&self, st: &St, s: usize) -> bool {
        match self.m.nodes[s].kind {
            Kind::Compound => self
                .m
                .get_child_states(s)
                .iter()
                .any(|&c| self.m.nodes[c].kind == Kind::Final && st.configuration.contains(&c)),
            Kind::Parallel => self
                .m
                .get_child_states(s)
                .iter()
                .all(|&c| self.is_in_final_state(st, c)),
            _ => false,
        }
    }

    fn add_descendant_states_to_enter(&self, st: &St, state: usize, out: &mut BTreeSet<usize>) {
        if self.m.nodes[state].kind == Kind::History {
            let parent = self.m.nodes[state].parent;
            let targets = match st.history_value.get(&state) {
                Some(v) => v.clone(),
                None => self.history_default(state),
            };
            for &s in &targets {
                self.add_descendant_states_to_enter(st, s, out);
            }
            for &s in &targets {
                self.add_ancestor_states_to_enter(st, s, parent, out);
            }
        } else {
            out.insert(state);
            match self.m.nodes[state].kind {
                Kind::Compound => {
                    let s = self.m.nodes[state].initial.expect("initial");
                    self.add_descendant_states_to_enter(st, s, out);
                    self.add_ancestor_states_to_enter(st, s, Some(state), out);
                }
                Kind::Parallel => {
                    for child in self.m.get_child_states(state) {
                        if !out.iter().any(|&s| self.m.is_descendant(s, child))
                            && !st.configuration.contains(&child)
                        {
                            self.add_descendant_states_to_enter(st, child, out);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn add_ancestor_states_to_enter(
        &self,
        st: &St,
        state: usize,
        ancestor: Option<usize>,
        out: &mut BTreeSet<usize>,
    ) {
        let mut ancestors = self.m.get_proper_ancestors(state, ancestor);
        for &anc in &ancestors {
            out.insert(anc);
        }
        // The root is never entered, but when a transition's domain is a parallel root, the
        // regions it emptied are entered again.
        if let Some(a) = ancestor
            && self.m.is_root(a)
            && self.m.nodes[a].kind == Kind::Parallel
        {
            ancestors.push(a);
        }
        for anc in ancestors {
            if self.m.nodes[anc].kind == Kind::Parallel {
                for child in self.m.get_child_states(anc) {
                    if !out.iter().any(|&s| self.m.is_descendant(s, child))
                        && !st.configuration.contains(&child)
                    {
                        self.add_descendant_states_to_enter(st, child, out);
                    }
                }
            }
        }
    }

    /// The rest of a macrostep after its first microstep.
    fn settle(&self, st: &mut St, mut microsteps: u32) -> Result<(), RefError> {
        while st.running {
            let mut enabled = self.select_transitions(st, None);
            if enabled.is_empty() {
                let Some(internal) = st.internal_queue.pop_front() else {
                    break;
                };
                st.event = internal;
                enabled = self.select_transitions(st, Some(&st.event.name.clone()));
            }
            if !enabled.is_empty() {
                microsteps += 1;
                if microsteps > self.m.def.max_microsteps {
                    return Err(RefError::Livelock);
                }
                self.microstep(st, &enabled)?;
            }
        }
        if st.running {
            let pending: Vec<usize> = std::mem::take(&mut st.states_to_invoke)
                .into_iter()
                .filter(|s| st.configuration.contains(s))
                .collect();
            for s in pending {
                for (id, name, params) in &self.m.nodes[s].invoke {
                    let params = match params {
                        Some(e) => self.eval(st, e)?,
                        None => Value::Null,
                    };
                    st.invocations.insert(
                        id.clone(),
                        Invocation {
                            state: self.m.nodes[s].path.clone(),
                            name: name.clone(),
                            params: params.clone(),
                        },
                    );
                    st.effects.push(Effect::Start {
                        id: id.clone(),
                        name: name.clone(),
                        params,
                    });
                }
            }
        } else {
            // Finished: nothing is left running or pending.
            for id in std::mem::take(&mut st.invocations).into_keys() {
                st.effects.push(Effect::Stop { id });
            }
            for id in std::mem::take(&mut st.timers).into_keys() {
                st.effects.push(Effect::Cancel { id });
            }
        }
        st.internal_queue.clear();
        st.states_to_invoke.clear();
        Ok(())
    }

    fn to_wire(&self, st: &St) -> WireSnapshot {
        let path = |s: &usize| self.m.nodes[*s].path.clone();
        WireSnapshot {
            schema: foldstate::SNAPSHOT_SCHEMA.into(),
            machine: self.machine.clone(),
            status: if st.running {
                Status::Active
            } else {
                Status::Done
            },
            configuration: st
                .configuration
                .iter()
                .filter(|s| !self.m.is_root(**s))
                .map(path)
                .collect(),
            history: st
                .history_value
                .iter()
                .map(|(h, v)| (path(h), v.iter().map(path).collect()))
                .collect(),
            context: st.context.clone(),
            timers: st.timers.clone(),
            invocations: st.invocations.clone(),
        }
    }

    fn read_wire(&self, w: &WireSnapshot) -> St {
        let mut configuration: BTreeSet<usize> =
            w.configuration.iter().map(|p| self.m.find(p)).collect();
        configuration.insert(0);
        St {
            configuration,
            history_value: w
                .history
                .iter()
                .map(|(h, v)| (self.m.find(h), v.iter().map(|p| self.m.find(p)).collect()))
                .collect(),
            context: w.context.clone(),
            timers: w.timers.clone(),
            invocations: w.invocations.clone(),
            running: w.status == Status::Active,
            internal_queue: VecDeque::new(),
            states_to_invoke: BTreeSet::new(),
            entered: BTreeMap::new(),
            clock: 0,
            effects: Vec::new(),
            event: Event::default(),
            now: None,
        }
    }

    /// Starts the machine: enters the root's default configuration and settles.
    pub fn start(&self, input: &Value) -> Result<(WireSnapshot, Vec<Effect>), RefError> {
        let mut context = match &self.m.def.context {
            Value::Null => Value::Object(Map::new()),
            other => other.clone(),
        };
        if let (Value::Object(c), Value::Object(i)) = (&mut context, input) {
            for (k, v) in i {
                c.insert(k.clone(), v.clone());
            }
        }
        let mut st = St {
            configuration: BTreeSet::new(),
            history_value: BTreeMap::new(),
            context,
            timers: BTreeMap::new(),
            invocations: BTreeMap::new(),
            running: true,
            internal_queue: VecDeque::new(),
            states_to_invoke: BTreeSet::new(),
            entered: BTreeMap::new(),
            clock: 0,
            effects: Vec::new(),
            event: Event {
                name: "foldstate.init".into(),
                data: input.clone(),
                at: None,
            },
            now: None,
        };
        let mut states_to_enter = BTreeSet::new();
        self.add_descendant_states_to_enter(&st, 0, &mut states_to_enter);
        self.enter(&mut st, &states_to_enter)?;
        self.settle(&mut st, 1)?;
        Ok((self.to_wire(&st), st.effects))
    }

    /// Processes one external event. Returns the new snapshot, the effects, and whether the
    /// event was handled (false: ignored).
    pub fn step(
        &self,
        snapshot: &WireSnapshot,
        event: &Event,
    ) -> Result<(WireSnapshot, Vec<Effect>, bool), RefError> {
        let mut st = self.read_wire(snapshot);
        if !st.running {
            return Err(RefError::Done);
        }
        st.event = event.clone();
        st.now = event.at;
        // Events the interpreter produces itself are not accepted from outside: `done.state`
        // never, a delayed transition's event only while its timer is pending.
        let pending = st.timers.values().any(|t| t.event == event.name);
        let own_timer = self
            .m
            .nodes
            .iter()
            .any(|n| n.after.iter().any(|(_, e, _)| *e == event.name));
        let reserved = event.name.starts_with("done.state.") || (own_timer && !pending);
        // A delivered timer is no longer pending.
        st.timers.retain(|_, t| t.event != event.name);
        let enabled = if reserved {
            Vec::new()
        } else {
            self.select_transitions(&st, Some(&event.name))
        };
        if enabled.is_empty() {
            let ignored = self.m.def.unhandled == Unhandled::Ignore
                || name_match(&self.m.def.ignore, &event.name);
            return if ignored {
                Ok((self.to_wire(&st), Vec::new(), false))
            } else {
                Err(RefError::Rejected)
            };
        }
        if self.m.def.max_microsteps == 0 {
            return Err(RefError::Livelock);
        }
        self.microstep(&mut st, &enabled)?;
        self.settle(&mut st, 1)?;
        Ok((self.to_wire(&st), st.effects, true))
    }
}

fn as_int(n: &Number) -> Option<i64> {
    n.as_i64()
}

fn arith(op: char, a: &Value, b: &Value) -> Result<Value, RefError> {
    let (Value::Number(x), Value::Number(y)) = (a, b) else {
        return Err(RefError::Eval);
    };
    if let (Some(x), Some(y)) = (as_int(x), as_int(y)) {
        let r = match op {
            '+' => x.checked_add(y),
            '-' => x.checked_sub(y),
            _ => x.checked_mul(y),
        };
        return r.map(Value::from).ok_or(RefError::Eval);
    }
    let (x, y) = (
        x.as_f64().ok_or(RefError::Eval)?,
        y.as_f64().ok_or(RefError::Eval)?,
    );
    let r = match op {
        '+' => x + y,
        '-' => x - y,
        _ => x * y,
    };
    Number::from_f64(r).map(Value::Number).ok_or(RefError::Eval)
}

fn order(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (as_int(x), as_int(y)) {
            (Some(x), Some(y)) => Some(x.cmp(&y)),
            _ => x.as_f64()?.partial_cmp(&y.as_f64()?),
        },
        (Value::String(x), Value::String(y)) => Some(x.cmp(y)),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(_), Value::Number(_)) => order(a, b) == Some(std::cmp::Ordering::Equal),
        _ => a == b,
    }
}

/// `context[pointer] = value`, creating missing objects.
fn assign(context: &mut Value, pointer: &str, value: Value) -> Result<(), RefError> {
    if pointer.is_empty() {
        *context = value;
        return Ok(());
    }
    let rest = pointer.strip_prefix('/').ok_or(RefError::Eval)?;
    let segments: Vec<String> = rest
        .split('/')
        .map(|s| s.replace("~1", "/").replace("~0", "~"))
        .collect();
    let mut cur = context;
    for (i, seg) in segments.iter().enumerate() {
        let last = i + 1 == segments.len();
        match cur {
            Value::Object(m) => {
                if last {
                    m.insert(seg.clone(), value);
                    return Ok(());
                }
                cur = m
                    .entry(seg.clone())
                    .or_insert_with(|| Value::Object(Map::new()));
            }
            Value::Array(a) => {
                let slot = seg
                    .parse::<usize>()
                    .ok()
                    .and_then(|n| a.get_mut(n))
                    .ok_or(RefError::Eval)?;
                if last {
                    *slot = value;
                    return Ok(());
                }
                cur = slot;
            }
            _ => return Err(RefError::Eval),
        }
    }
    Ok(())
}

// SPDX-License-Identifier: Apache-2.0
//! A compiled definition: validated, with states numbered in document order so that the
//! descendants of a state are a contiguous range of numbers.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use serde_json::Value;

use crate::canon;
use crate::def::{Action, CmpOp, Definition, Expr, Guard, Kind, SCHEMA, StateDef, TransitionDef};
use crate::error::{DefinitionError, DefinitionErrorKind as K};
use crate::wire::TransitionRef;

/// Number of a state in document order. The root is 0.
pub(crate) type StateId = u32;
/// Number of a transition in document order.
pub(crate) type TransId = u32;

pub(crate) const ROOT: StateId = 0;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Desc {
    Any,
    Exact(String),
    /// `p.*`: `p` itself and every name that continues it after a dot.
    Prefix(String),
}

impl Desc {
    fn parse(s: &str) -> Option<Desc> {
        if s == "*" {
            return Some(Desc::Any);
        }
        let (body, prefix) = match s.strip_suffix(".*") {
            Some(p) => (p, true),
            None => (s, false),
        };
        if body.is_empty() || body.contains('*') || body.split('.').any(str::is_empty) {
            return None;
        }
        Some(if prefix {
            Desc::Prefix(body.to_string())
        } else {
            Desc::Exact(body.to_string())
        })
    }

    pub(crate) fn matches(&self, name: &str) -> bool {
        match self {
            Desc::Any => true,
            Desc::Exact(d) => d == name,
            Desc::Prefix(p) => {
                name == p || (name.starts_with(p.as_str()) && name[p.len()..].starts_with('.'))
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum CGuard {
    In(StateId),
    Not(Box<CGuard>),
    All(Vec<CGuard>),
    Any(Vec<CGuard>),
    Cmp { left: Expr, op: CmpOp, right: Expr },
    Named { name: String, params: Value },
}

/// Where a transition's domain comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Domain {
    /// No targets: nothing is exited or entered.
    None,
    Fixed(StateId),
    /// A target is a history state, so the domain depends on what was recorded.
    Dynamic,
}

#[derive(Debug, Clone)]
pub(crate) struct Trans {
    pub source: StateId,
    /// Position in the source's list (`on`, then `after`).
    pub index: u32,
    /// `None`: eventless.
    pub desc: Option<Vec<Desc>>,
    pub guard: Option<CGuard>,
    /// The guard and the event as written, for inspection.
    pub def_guard: Option<Guard>,
    pub event_text: Option<String>,
    /// `after` transitions: the delay.
    pub delay_ms: Option<u64>,
    pub targets: Vec<StateId>,
    pub internal: bool,
    pub actions: Vec<Action>,
    pub domain: Domain,
}

#[derive(Debug, Clone)]
pub(crate) struct After {
    pub id: String,
    pub event: String,
    pub delay_ms: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct Invoke {
    pub id: String,
    pub name: String,
    pub params: Option<Expr>,
}

#[derive(Debug, Clone)]
pub(crate) struct State {
    pub path: String,
    pub kind: Kind,
    pub parent: Option<StateId>,
    /// Child states, without history pseudo-states.
    pub children: Vec<StateId>,
    pub history: Vec<StateId>,
    /// The last descendant; the state's descendants are `self + 1 ..= last`.
    pub last: StateId,
    pub depth: u32,
    pub initial: Option<StateId>,
    /// History states: the default targets.
    pub default: Vec<StateId>,
    pub deep: bool,
    pub trans: Vec<TransId>,
    pub eventless: Vec<TransId>,
    pub entry: Vec<Action>,
    pub exit: Vec<Action>,
    pub after: Vec<After>,
    pub invoke: Vec<Invoke>,
    pub tags: Vec<String>,
    pub meta: Value,
}

/// Upper bounds for definitions from sources you do not control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_states: usize,
    pub max_transitions: usize,
    pub max_depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_states: 1_000_000,
            max_transitions: 4_000_000,
            max_depth: 64,
        }
    }
}

/// A definition ready to run. Compile once, then use it for any number of snapshots and logs.
#[derive(Debug, Clone)]
pub struct Machine {
    pub(crate) def: Definition,
    pub(crate) hash: [u8; 32],
    pub(crate) hash_hex: String,
    pub(crate) states: Vec<State>,
    pub(crate) trans: Vec<Trans>,
    pub(crate) by_path: BTreeMap<String, StateId>,
    /// Transitions by exact event name, in document order.
    pub(crate) exact: BTreeMap<String, Vec<TransId>>,
    /// Transitions with a `p.*` or `*` descriptor, in document order.
    pub(crate) patterns: Vec<TransId>,
    pub(crate) has_eventless: bool,
    pub(crate) ignore: Vec<Desc>,
    /// Eventless transitions whose guard reads the context, the event or the host: they
    /// are looked at again after every microstep.
    pub(crate) volatile: Vec<TransId>,
    /// Per state: the eventless transitions whose guard asks whether that state is active.
    /// They are looked at again when the state is entered or exited.
    pub(crate) watchers: Vec<Vec<TransId>>,
    /// Names of the events that `after` timers deliver.
    pub(crate) after_events: alloc::collections::BTreeSet<String>,
}

struct Builder<'a> {
    limits: &'a Limits,
    states: Vec<State>,
    defs: Vec<&'a StateDef>,
    by_path: BTreeMap<String, StateId>,
}

fn err<T>(kind: K, at: impl Into<String>) -> Result<T, DefinitionError> {
    Err(DefinitionError {
        kind,
        at: at.into(),
    })
}

fn child_path(parent: &State, id: &str) -> String {
    if parent.parent.is_none() {
        id.to_string()
    } else {
        format!("{}.{}", parent.path, id)
    }
}

impl<'a> Builder<'a> {
    fn add(
        &mut self,
        d: &'a StateDef,
        parent: Option<StateId>,
        path: String,
        depth: usize,
    ) -> Result<StateId, DefinitionError> {
        if depth > self.limits.max_depth {
            return err(K::TooLarge("levels"), path);
        }
        if self.states.len() >= self.limits.max_states {
            return err(K::TooLarge("states"), path);
        }
        let kind = d.resolved_kind();
        let id = self.states.len() as StateId;
        let has_children = !d.states.is_empty();
        match kind {
            Kind::Atomic | Kind::Final | Kind::History if has_children => {
                return err(K::UnexpectedChildren, path);
            }
            Kind::Compound | Kind::Parallel
                if !d.states.iter().any(|c| c.resolved_kind() != Kind::History) =>
            {
                return err(K::NoChildren, path);
            }
            _ => {}
        }
        if kind == Kind::Final && !(d.on.is_empty() && d.after.is_empty() && d.invoke.is_empty()) {
            return err(K::FinalWithTransitions, path);
        }
        if kind == Kind::History
            && !(d.on.is_empty()
                && d.after.is_empty()
                && d.invoke.is_empty()
                && d.entry.is_empty()
                && d.exit.is_empty()
                && d.initial.is_none())
        {
            return err(K::HistoryWithContent, path);
        }
        if let Some(p) = parent {
            let pk = self.states[p as usize].kind;
            if kind == Kind::Final && pk != Kind::Compound {
                return err(K::FinalNeedsCompoundParent, path);
            }
            if kind == Kind::History && !matches!(pk, Kind::Compound | Kind::Parallel) {
                return err(K::BadHistory, path);
            }
        }
        self.states.push(State {
            path: path.clone(),
            kind,
            parent,
            children: Vec::new(),
            history: Vec::new(),
            last: id,
            depth: depth as u32,
            initial: None,
            default: Vec::new(),
            deep: d.deep,
            trans: Vec::new(),
            eventless: Vec::new(),
            entry: d.entry.clone(),
            exit: d.exit.clone(),
            after: Vec::new(),
            invoke: Vec::new(),
            tags: d.tags.clone(),
            meta: d.meta.clone(),
        });
        self.defs.push(d);
        if parent.is_some() {
            self.by_path.insert(path.clone(), id);
        }
        let mut seen: Vec<&str> = Vec::new();
        for c in &d.states {
            if c.id.is_empty() || c.id.contains(['.', '#']) || c.id.contains(char::is_whitespace) {
                return err(K::BadStateId(c.id.clone()), path);
            }
            if seen.contains(&c.id.as_str()) {
                return err(K::DuplicateStateId(c.id.clone()), path);
            }
            seen.push(&c.id);
            let cp = child_path(&self.states[id as usize], &c.id);
            let ci = self.add(c, Some(id), cp, depth + 1)?;
            let me = &mut self.states[id as usize];
            if c.resolved_kind() == Kind::History {
                me.history.push(ci);
            } else {
                me.children.push(ci);
            }
        }
        self.states[id as usize].last = (self.states.len() - 1) as StateId;
        Ok(id)
    }

    /// Resolves a state reference written in state `from`.
    fn resolve(&self, from: StateId, reference: &str) -> Option<StateId> {
        let s = &self.states[from as usize];
        if let Some(abs) = reference.strip_prefix('#') {
            self.by_path.get(abs).copied()
        } else if let Some(rel) = reference.strip_prefix('.') {
            if s.parent.is_none() {
                self.by_path.get(rel).copied()
            } else {
                self.by_path.get(&format!("{}.{}", s.path, rel)).copied()
            }
        } else {
            let parent = &self.states[s.parent? as usize];
            self.by_path.get(&child_path(parent, reference)).copied()
        }
    }

    fn is_descendant(&self, s: StateId, of: StateId) -> bool {
        of < s && s <= self.states[of as usize].last
    }

    fn guard(&self, from: StateId, g: &Guard, at: &str) -> Result<CGuard, DefinitionError> {
        Ok(match g {
            Guard::In(r) => match self.resolve(from, r) {
                Some(s) => CGuard::In(s),
                None => return err(K::UnknownState(r.clone()), at),
            },
            Guard::Not(g) => CGuard::Not(Box::new(self.guard(from, g, at)?)),
            Guard::All(gs) => CGuard::All(
                gs.iter()
                    .map(|g| self.guard(from, g, at))
                    .collect::<Result<_, _>>()?,
            ),
            Guard::Any(gs) => CGuard::Any(
                gs.iter()
                    .map(|g| self.guard(from, g, at))
                    .collect::<Result<_, _>>()?,
            ),
            Guard::Cmp { left, op, right } => {
                check_expr(left, at)?;
                check_expr(right, at)?;
                CGuard::Cmp {
                    left: left.clone(),
                    op: *op,
                    right: right.clone(),
                }
            }
            Guard::Named { name, params } => CGuard::Named {
                name: name.clone(),
                params: params.clone(),
            },
        })
    }

    fn targets(
        &self,
        from: StateId,
        refs: &[String],
        at: &str,
    ) -> Result<Vec<StateId>, DefinitionError> {
        let mut out: Vec<StateId> = Vec::new();
        for r in refs {
            if r == "#" || r == "." && from == ROOT {
                return err(K::TargetsRoot, at);
            }
            let Some(s) = self.resolve(from, r) else {
                return err(K::UnknownState(r.clone()), at);
            };
            if !out.contains(&s) {
                out.push(s);
            }
        }
        for (i, &a) in out.iter().enumerate() {
            for &b in &out[i + 1..] {
                if !self.compatible(a, b) {
                    return err(
                        K::ConflictingTargets(
                            self.states[a as usize].path.clone(),
                            self.states[b as usize].path.clone(),
                        ),
                        at,
                    );
                }
            }
        }
        Ok(out)
    }

    /// Whether two targets of one transition are allowed together: neither contains the
    /// other and they meet in a parallel state. A history state stands for its parent,
    /// because what it restores can be anything below the parent.
    fn compatible(&self, a: StateId, b: StateId) -> bool {
        let stand_in = |s: StateId| {
            let st = &self.states[s as usize];
            match (st.kind, st.parent) {
                (Kind::History, Some(p)) => p,
                _ => s,
            }
        };
        let (x, y) = (stand_in(a), stand_in(b));
        if x == y || self.is_descendant(y, x) || self.is_descendant(x, y) {
            return false;
        }
        // Neither contains the other: fine when they meet in a parallel state.
        let mut p = self.states[x as usize].parent;
        while let Some(anc) = p {
            if self.is_descendant(y, anc) {
                return self.states[anc as usize].kind == Kind::Parallel;
            }
            p = self.states[anc as usize].parent;
        }
        false
    }
}

fn check_pointer(p: &str, at: &str) -> Result<(), DefinitionError> {
    if p.is_empty() || p.starts_with('/') {
        Ok(())
    } else {
        err(K::BadPointer(p.to_string()), at)
    }
}

fn check_expr(e: &Expr, at: &str) -> Result<(), DefinitionError> {
    match e {
        Expr::Lit(_) => Ok(()),
        Expr::Ctx(p) | Expr::Event(p) => check_pointer(p, at),
        Expr::Add(a, b) | Expr::Sub(a, b) | Expr::Mul(a, b) => {
            check_expr(a, at)?;
            check_expr(b, at)
        }
    }
}

fn check_actions(actions: &[Action], at: &str) -> Result<(), DefinitionError> {
    for a in actions {
        match a {
            Action::Assign { path, value } => {
                check_pointer(path, at)?;
                check_expr(value, at)?;
            }
            Action::Raise { event, data } => {
                if Desc::parse(event).is_none_or(|d| !matches!(d, Desc::Exact(_))) {
                    return err(K::BadEventDescriptor(event.clone()), at);
                }
                if let Some(e) = data {
                    check_expr(e, at)?;
                }
            }
            Action::Emit { params, .. } => {
                if let Some(e) = params {
                    check_expr(e, at)?;
                }
            }
            Action::Schedule { event, .. } => {
                if Desc::parse(event).is_none_or(|d| !matches!(d, Desc::Exact(_))) {
                    return err(K::BadEventDescriptor(event.clone()), at);
                }
            }
            Action::Cancel { .. } => {}
        }
    }
    Ok(())
}

fn descriptors(event: &str, at: &str) -> Result<Vec<Desc>, DefinitionError> {
    let list: Option<Vec<Desc>> = event.split_whitespace().map(Desc::parse).collect();
    match list {
        Some(l) if !l.is_empty() => Ok(l),
        _ => err(K::BadEventDescriptor(event.to_string()), at),
    }
}

impl Machine {
    /// Validates and compiles a definition.
    pub fn compile(def: &Definition) -> Result<Machine, DefinitionError> {
        Machine::compile_with(def, &Limits::default())
    }

    /// Like [`Machine::compile`], refusing definitions larger than `limits`.
    pub fn compile_with(def: &Definition, limits: &Limits) -> Result<Machine, DefinitionError> {
        if def.schema != SCHEMA {
            return err(K::UnknownSchema(def.schema.clone()), "");
        }
        if !(def.context.is_object() || def.context.is_null()) {
            return err(K::BadContext, "");
        }
        if !matches!(def.root.resolved_kind(), Kind::Compound | Kind::Parallel) {
            return err(K::BadRoot, "");
        }
        let mut b = Builder {
            limits,
            states: Vec::new(),
            defs: Vec::new(),
            by_path: BTreeMap::new(),
        };
        b.add(&def.root, None, String::new(), 0)?;

        let mut trans: Vec<Trans> = Vec::new();
        let mut ids: Vec<&str> = Vec::new();
        let mut timer_ids: Vec<String> = Vec::new();
        let mut invoke_ids: Vec<String> = Vec::new();
        for i in 0..b.states.len() {
            let sid = i as StateId;
            let d = b.defs[i];
            let path = b.states[i].path.clone();
            ids.clear();

            // Default entry.
            if b.states[i].kind == Kind::Compound {
                let initial = match &d.initial {
                    Some(name) => {
                        let found = b.states[i].children.iter().copied().find(|&c| {
                            b.states[c as usize].path.rsplit('.').next() == Some(name.as_str())
                        });
                        match found {
                            Some(c) => c,
                            None => return err(K::BadInitial(name.clone()), path),
                        }
                    }
                    None => b.states[i].children[0],
                };
                b.states[i].initial = Some(initial);
            } else if let Some(name) = &d.initial {
                return err(K::BadInitial(name.clone()), path);
            }
            if b.states[i].kind == Kind::History {
                let at = format!("{path} default");
                let parent = b.states[i].parent.unwrap_or(ROOT);
                let mut default = Vec::new();
                for r in &d.default {
                    match b.resolve(sid, r) {
                        Some(t)
                            if b.is_descendant(t, parent)
                                && b.states[t as usize].kind != Kind::History =>
                        {
                            default.push(t)
                        }
                        Some(_) => return err(K::BadHistoryDefault(r.clone()), at),
                        None => return err(K::UnknownState(r.clone()), at),
                    }
                }
                for (n, &x) in default.iter().enumerate() {
                    for &y in &default[n + 1..] {
                        if !b.compatible(x, y) {
                            return err(
                                K::ConflictingTargets(
                                    b.states[x as usize].path.clone(),
                                    b.states[y as usize].path.clone(),
                                ),
                                at,
                            );
                        }
                    }
                }
                b.states[i].default = default;
            } else if !d.default.is_empty() {
                return err(K::HistoryWithContent, path);
            }

            check_actions(&d.entry, &format!("{path} entry"))?;
            check_actions(&d.exit, &format!("{path} exit"))?;

            // Transitions: `on`, then the ones `after` stands for.
            let mut list: Vec<(TransitionDef, String, Option<u64>)> = Vec::new();
            for (n, t) in d.on.iter().enumerate() {
                list.push((t.clone(), format!("{path} on[{n}]"), None));
            }
            for (n, a) in d.after.iter().enumerate() {
                let id = a.id.clone().unwrap_or_else(|| format!("{path}:after:{n}"));
                let at = format!("{path} after[{n}]");
                if id.is_empty() || id.contains(char::is_whitespace) || id.contains('*') {
                    return err(K::BadEventDescriptor(id), at);
                }
                if timer_ids.contains(&id) {
                    return err(K::DuplicateId(id), at);
                }
                let event = format!("after.{id}");
                if Desc::parse(&event).is_none() {
                    return err(K::BadEventDescriptor(event), at);
                }
                timer_ids.push(id.clone());
                b.states[i].after.push(After {
                    id,
                    event: event.clone(),
                    delay_ms: a.delay_ms,
                });
                list.push((
                    TransitionDef {
                        event: Some(event),
                        guard: a.guard.clone(),
                        target: a.target.clone(),
                        internal: a.internal,
                        actions: a.actions.clone(),
                    },
                    at,
                    Some(a.delay_ms),
                ));
            }
            for (n, v) in d.invoke.iter().enumerate() {
                let id = v.id.clone().unwrap_or_else(|| format!("{path}:invoke:{n}"));
                let at = format!("{path} invoke[{n}]");
                if invoke_ids.contains(&id) {
                    return err(K::DuplicateId(id), at);
                }
                if let Some(e) = &v.params {
                    check_expr(e, &at)?;
                }
                invoke_ids.push(id.clone());
                b.states[i].invoke.push(Invoke {
                    id,
                    name: v.name.clone(),
                    params: v.params.clone(),
                });
            }
            for (index, (t, at, delay_ms)) in list.into_iter().enumerate() {
                if trans.len() >= limits.max_transitions {
                    return err(K::TooLarge("transitions"), at);
                }
                let desc = match &t.event {
                    Some(e) => Some(descriptors(e, &at)?),
                    None => None,
                };
                let guard = match &t.guard {
                    Some(g) => Some(b.guard(sid, g, &at)?),
                    None => None,
                };
                let targets = b.targets(sid, &t.target, &at)?;
                check_actions(&t.actions, &at)?;
                let tid = trans.len() as TransId;
                let dynamic = targets
                    .iter()
                    .any(|&x| b.states[x as usize].kind == Kind::History);
                let mut tr = Trans {
                    source: sid,
                    index: index as u32,
                    desc,
                    guard,
                    def_guard: t.guard,
                    event_text: t.event,
                    delay_ms,
                    targets,
                    internal: t.internal,
                    actions: t.actions,
                    domain: Domain::None,
                };
                if !tr.targets.is_empty() {
                    tr.domain = if dynamic {
                        Domain::Dynamic
                    } else {
                        Domain::Fixed(fixed_domain(&b.states, &tr, &tr.targets))
                    };
                }
                if tr.desc.is_none() {
                    b.states[i].eventless.push(tid);
                }
                b.states[i].trans.push(tid);
                trans.push(tr);
            }
        }

        let mut exact: BTreeMap<String, Vec<TransId>> = BTreeMap::new();
        let mut patterns = Vec::new();
        let mut has_eventless = false;
        for (tid, t) in trans.iter().enumerate() {
            let tid = tid as TransId;
            match &t.desc {
                None => has_eventless = true,
                Some(ds) => {
                    let mut pattern = false;
                    for d in ds {
                        match d {
                            Desc::Exact(name) => {
                                let v = exact.entry(name.clone()).or_default();
                                if v.last() != Some(&tid) {
                                    v.push(tid);
                                }
                            }
                            _ => pattern = true,
                        }
                    }
                    if pattern {
                        patterns.push(tid);
                    }
                }
            }
        }
        // What can make an eventless transition become enabled.
        let mut volatile = Vec::new();
        let mut watchers: Vec<Vec<TransId>> = alloc::vec![Vec::new(); b.states.len()];
        for (tid, t) in trans.iter().enumerate() {
            if t.desc.is_some() {
                continue;
            }
            let Some(g) = &t.guard else { continue };
            let mut states = Vec::new();
            if guard_inputs(g, &mut states) {
                volatile.push(tid as TransId);
            }
            states.sort_unstable();
            states.dedup();
            for s in states {
                watchers[s as usize].push(tid as TransId);
            }
        }
        let mut ignore = Vec::new();
        for d in &def.ignore {
            match Desc::parse(d) {
                Some(d) => ignore.push(d),
                None => return err(K::BadEventDescriptor(d.clone()), "ignore"),
            }
        }

        // Through text rather than `to_value`: it reuses the serialiser every caller that
        // writes JSON already has, which keeps binaries smaller.
        let json: Value = serde_json::to_string(def)
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or(Value::Null);
        let hash = canon::sha256(canon::canonical_json(&json).as_bytes());
        Ok(Machine {
            def: def.clone(),
            hash,
            hash_hex: canon::hex(&hash),
            states: b.states,
            trans,
            by_path: b.by_path,
            exact,
            patterns,
            has_eventless,
            ignore,
            volatile,
            watchers,
            after_events: timer_ids.iter().map(|id| format!("after.{id}")).collect(),
        })
    }

    /// SHA-256 (hex) of the canonical JSON of the definition. Snapshots and logs carry it.
    pub fn hash(&self) -> &str {
        &self.hash_hex
    }

    /// The definition this machine was compiled from.
    pub fn definition(&self) -> &Definition {
        &self.def
    }

    pub(crate) fn state(&self, s: StateId) -> &State {
        &self.states[s as usize]
    }

    pub(crate) fn tr(&self, t: TransId) -> &Trans {
        &self.trans[t as usize]
    }

    pub(crate) fn is_descendant(&self, s: StateId, of: StateId) -> bool {
        of < s && s <= self.states[of as usize].last
    }

    pub(crate) fn is_atomic(&self, s: StateId) -> bool {
        matches!(self.states[s as usize].kind, Kind::Atomic | Kind::Final)
    }

    pub(crate) fn state_id(&self, path: &str) -> Option<StateId> {
        if path.is_empty() {
            Some(ROOT)
        } else {
            self.by_path.get(path).copied()
        }
    }

    pub(crate) fn trans_ref(&self, t: TransId) -> TransitionRef {
        let tr = self.tr(t);
        TransitionRef {
            source: self.state(tr.source).path.clone(),
            index: tr.index,
        }
    }

    pub(crate) fn trans_id(&self, r: &TransitionRef) -> Option<TransId> {
        let s = self.state_id(&r.source)?;
        self.state(s).trans.get(r.index as usize).copied()
    }

    /// The domain of a transition given its effective targets (history already replaced by
    /// what it restores).
    pub(crate) fn domain_for(&self, t: &Trans, effective: &[StateId]) -> StateId {
        fixed_domain(&self.states, t, effective)
    }

    /// Names of the host guards the definition refers to, sorted, each once.
    pub fn named_guards(&self) -> Vec<&str> {
        fn walk<'a>(g: &'a CGuard, out: &mut Vec<&'a str>) {
            match g {
                CGuard::Named { name, .. } => out.push(name),
                CGuard::Not(g) => walk(g, out),
                CGuard::All(gs) | CGuard::Any(gs) => gs.iter().for_each(|g| walk(g, out)),
                CGuard::In(_) | CGuard::Cmp { .. } => {}
            }
        }
        let mut out = Vec::new();
        for t in &self.trans {
            if let Some(g) = &t.guard {
                walk(g, &mut out);
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// Collects the states a guard asks about; returns true when it also reads the context, the
/// event or the host.
fn guard_inputs(g: &CGuard, states: &mut Vec<StateId>) -> bool {
    match g {
        CGuard::In(s) => {
            states.push(*s);
            false
        }
        CGuard::Not(g) => guard_inputs(g, states),
        CGuard::All(gs) | CGuard::Any(gs) => {
            let mut volatile = false;
            for g in gs {
                volatile |= guard_inputs(g, states);
            }
            volatile
        }
        CGuard::Cmp { .. } | CGuard::Named { .. } => true,
    }
}

/// See "Taking transitions" in `docs/semantics.md`.
fn fixed_domain(states: &[State], t: &Trans, targets: &[StateId]) -> StateId {
    let inside = |s: StateId, of: StateId| of < s && s <= states[of as usize].last;
    let source = &states[t.source as usize];
    if t.internal && source.kind == Kind::Compound && targets.iter().all(|&x| inside(x, t.source)) {
        return t.source;
    }
    let mut p = source.parent;
    while let Some(anc) = p {
        let a = &states[anc as usize];
        if (a.kind == Kind::Compound || a.parent.is_none())
            && targets.iter().all(|&x| inside(x, anc))
        {
            return anc;
        }
        p = a.parent;
    }
    // The source is the root.
    ROOT
}

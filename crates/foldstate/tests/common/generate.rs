// SPDX-License-Identifier: Apache-2.0
//! Random definitions and event sequences, reproducible from a seed.
//!
//! Every generated definition is valid. The generator aims at the corners of the semantics:
//! nested parallel states, history, transitions between regions, several targets, eventless
//! transitions, raised events, timers, invocations, final states, and actions that fail.

use foldstate::{
    Action, AfterDef, CmpOp, Definition, Event, Expr, Guard, InvokeDef, Kind, StateDef,
    TransitionDef, Unhandled, WireSnapshot,
};
use serde_json::json;

/// xorshift64*: small, fast and the same everywhere.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    /// True with probability `percent` / 100.
    pub fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len())]
    }
}

pub const EVENTS: [&str; 5] = ["a", "b", "c", "d", "a.x"];

#[derive(Clone)]
struct Info {
    path: String,
    kind: Kind,
}

struct Gen<'r> {
    rng: &'r mut Rng,
    counter: usize,
    max_depth: usize,
}

impl Gen<'_> {
    fn id(&mut self) -> String {
        self.counter += 1;
        format!("s{}", self.counter)
    }

    fn state(&mut self, depth: usize, allow_final: bool) -> StateDef {
        let id = self.id();
        let roll = self.rng.below(100);
        if allow_final && roll < 12 {
            return StateDef::final_state(id);
        }
        if depth >= self.max_depth || roll < 55 {
            return StateDef::atomic(id);
        }
        if roll < 82 {
            self.compound(id, depth)
        } else {
            self.parallel(id, depth)
        }
    }

    fn compound(&mut self, id: String, depth: usize) -> StateDef {
        let n = 2 + self.rng.below(2);
        let mut states: Vec<StateDef> = (0..n).map(|_| self.state(depth + 1, true)).collect();
        if states.iter().all(|s| s.kind == Some(Kind::Final)) {
            states[0] = StateDef::atomic(self.id());
        }
        let real: Vec<String> = states.iter().map(|s| s.id.clone()).collect();
        if self.rng.chance(30) {
            let deep = self.rng.chance(50);
            states.insert(
                self.rng.below(states.len() + 1),
                StateDef::history(self.id(), deep),
            );
        }
        let mut s = StateDef::compound(id, states);
        if self.rng.chance(40) {
            s.initial = Some(self.rng.pick(&real).clone());
        }
        s
    }

    fn parallel(&mut self, id: String, depth: usize) -> StateDef {
        let n = 2 + self.rng.below(2);
        let mut states: Vec<StateDef> = (0..n).map(|_| self.state(depth + 1, false)).collect();
        if self.rng.chance(15) {
            let deep = self.rng.chance(50);
            states.push(StateDef::history(self.id(), deep));
        }
        StateDef::parallel(id, states)
    }

    fn expr_n(&mut self) -> Expr {
        Expr::Ctx("/n".into())
    }

    fn guard(&mut self, states: &[Info], depth: usize) -> Guard {
        let target = format!("#{}", self.rng.pick(states).path);
        match self.rng.below(100) {
            0..=34 => Guard::In(target),
            35..=49 => Guard::Not(Box::new(Guard::In(target))),
            50..=79 => Guard::Cmp {
                left: self.expr_n(),
                op: *self.rng.pick(&[
                    CmpOp::Eq,
                    CmpOp::Ne,
                    CmpOp::Lt,
                    CmpOp::Le,
                    CmpOp::Gt,
                    CmpOp::Ge,
                ]),
                right: Expr::Lit(json!(self.rng.below(4))),
            },
            80..=89 => Guard::Named {
                name: (*self.rng.pick(&["even", "odd", "missing"])).into(),
                params: serde_json::Value::Null,
            },
            90..=92 => Guard::Cmp {
                // Not evaluable once /last is a string or null: the guard is then false.
                left: Expr::Add(
                    Box::new(Expr::Ctx("/last".into())),
                    Box::new(Expr::Lit(json!(1))),
                ),
                op: CmpOp::Gt,
                right: Expr::Lit(json!(1)),
            },
            _ if depth < 2 => {
                let a = self.guard(states, depth + 1);
                let b = self.guard(states, depth + 1);
                if self.rng.chance(50) {
                    Guard::All(vec![a, b])
                } else {
                    Guard::Any(vec![a, b])
                }
            }
            _ => Guard::In(target),
        }
    }

    fn action(&mut self) -> Action {
        match self.rng.below(100) {
            0..=29 => Action::Assign {
                path: "/n".into(),
                value: Expr::Add(Box::new(self.expr_n()), Box::new(Expr::Lit(json!(1)))),
            },
            30..=41 => Action::Assign {
                path: "/last".into(),
                value: Expr::Event("/v".into()),
            },
            42..=47 => Action::Assign {
                path: format!("/seen/{}", self.rng.pick(&["x", "y~0", "z~1w"])),
                value: Expr::Sub(Box::new(self.expr_n()), Box::new(Expr::Lit(json!(0.5)))),
            },
            48..=62 => Action::Raise {
                event: (*self.rng.pick(&["a", "b", "c", "d", "i", "i.j"])).into(),
                data: self
                    .rng
                    .chance(50)
                    .then(|| Expr::Lit(json!({"v": "raised"}))),
            },
            63..=77 => Action::Emit {
                name: format!("e{}", self.rng.below(3)),
                params: self.rng.chance(60).then(|| self.expr_n()),
            },
            78..=87 => Action::Schedule {
                id: format!("t{}", self.rng.below(3)),
                event: (*self.rng.pick(&["a", "b", "tick"])).into(),
                delay_ms: 10 * self.rng.below(5) as u64,
            },
            88..=94 => Action::Cancel {
                id: format!("t{}", self.rng.below(3)),
            },
            95..=96 => Action::Assign {
                // Overflows after a few rounds.
                path: "/n".into(),
                value: Expr::Mul(
                    Box::new(self.expr_n()),
                    Box::new(Expr::Lit(json!(3_037_000_500i64))),
                ),
            },
            97..=98 => Action::Assign {
                // /n is a number, so this write fails.
                path: "/n/x".into(),
                value: Expr::Lit(json!(1)),
            },
            _ => Action::Assign {
                path: "/big".into(),
                value: Expr::Mul(
                    Box::new(Expr::Lit(json!(1e200))),
                    Box::new(Expr::Lit(json!(1e200))),
                ),
            },
        }
    }

    fn actions(&mut self, max: usize) -> Vec<Action> {
        (0..self.rng.below(max + 1))
            .map(|_| self.action())
            .collect()
    }

    /// One or two targets that can be active together, or none.
    fn targets(&mut self, states: &[Info], tree: &StateDef) -> Vec<String> {
        let roll = self.rng.below(100);
        if roll < 8 {
            return Vec::new();
        }
        let first = self.rng.pick(states).path.clone();
        if roll < 22 {
            for _ in 0..8 {
                let second = self.rng.pick(states).path.clone();
                if second != first && compatible(tree, &first, &second) {
                    return vec![format!("#{first}"), format!("#{second}")];
                }
            }
        }
        vec![format!("#{first}")]
    }

    fn decorate(
        &mut self,
        s: &mut StateDef,
        path: &str,
        states: &[Info],
        tree: &StateDef,
        root: bool,
    ) {
        let kind = s.resolved_kind();
        if kind == Kind::History {
            // Defaults must be descendants of the parent.
            let parent = path.rsplit_once('.').map(|(p, _)| p).unwrap_or("");
            let below: Vec<&Info> = states
                .iter()
                .filter(|i| {
                    i.kind != Kind::History
                        && (parent.is_empty() || i.path.starts_with(&format!("{parent}.")))
                })
                .collect();
            if !below.is_empty() && self.rng.chance(50) {
                s.default = vec![format!("#{}", below[self.rng.below(below.len())].path)];
            }
            return;
        }
        if kind != Kind::Final {
            let n = if root {
                self.rng.below(2)
            } else {
                self.rng.below(4)
            };
            for _ in 0..n {
                let eventless = self.rng.chance(12);
                let mut t = TransitionDef {
                    event: (!eventless).then(|| match self.rng.below(100) {
                        0..=3 => "*".to_string(),
                        4..=9 => format!("{} {}", self.rng.pick(&EVENTS), self.rng.pick(&EVENTS)),
                        10..=14 => "i".to_string(),
                        15..=17 => "done.state.*".to_string(),
                        18..=20 => "a.*".to_string(),
                        _ => (*self.rng.pick(&EVENTS)).to_string(),
                    }),
                    guard: None,
                    target: self.targets(states, tree),
                    internal: self.rng.chance(20),
                    actions: self.actions(2),
                };
                if eventless || self.rng.chance(35) {
                    t.guard = Some(self.guard(states, 0));
                }
                if eventless && self.rng.chance(70) {
                    // Bound most eventless loops by a counter.
                    t.guard = Some(Guard::Cmp {
                        left: self.expr_n(),
                        op: CmpOp::Lt,
                        right: Expr::Lit(json!(self.rng.below(6))),
                    });
                    t.actions.insert(
                        0,
                        Action::Assign {
                            path: "/n".into(),
                            value: Expr::Add(
                                Box::new(self.expr_n()),
                                Box::new(Expr::Lit(json!(1))),
                            ),
                        },
                    );
                }
                s.on.push(t);
            }
            if !root && self.rng.chance(10) {
                s.after.push(AfterDef {
                    delay_ms: 100,
                    id: self.rng.chance(30).then(|| format!("after-{path}")),
                    guard: None,
                    target: self.targets(states, tree),
                    internal: false,
                    actions: self.actions(1),
                });
            }
            if self.rng.chance(10) {
                s.invoke.push(InvokeDef {
                    id: None,
                    name: "job".into(),
                    params: self.rng.chance(50).then(|| self.expr_n()),
                });
            }
        }
        if self.rng.chance(20) {
            s.entry = self.actions(2);
        }
        if self.rng.chance(20) {
            s.exit = self.actions(2);
        }
        if self.rng.chance(10) {
            s.tags.push("tagged".into());
        }
    }
}

fn collect(s: &StateDef, path: &str, root: bool, out: &mut Vec<Info>) {
    if !root {
        out.push(Info {
            path: path.to_string(),
            kind: s.resolved_kind(),
        });
    }
    for c in &s.states {
        let p = if root || path.is_empty() {
            c.id.clone()
        } else {
            format!("{path}.{}", c.id)
        };
        collect(c, &p, false, out);
    }
}

fn node<'a>(tree: &'a StateDef, path: &[&str]) -> &'a StateDef {
    path.iter().fold(tree, |s, id| {
        s.states.iter().find(|c| c.id == *id).expect("path exists")
    })
}

/// Two targets are allowed together when neither contains the other and they meet in a
/// parallel state. A history state stands for its parent.
fn compatible(tree: &StateDef, a: &str, b: &str) -> bool {
    let stand_in = |p: &str| -> Vec<String> {
        let mut segs: Vec<String> = p.split('.').map(String::from).collect();
        let refs: Vec<&str> = segs.iter().map(String::as_str).collect();
        if node(tree, &refs).resolved_kind() == Kind::History {
            segs.pop();
        }
        segs
    };
    let (pa, pb) = (stand_in(a), stand_in(b));
    let common = pa.iter().zip(&pb).take_while(|(x, y)| x == y).count();
    if common == pa.len() || common == pb.len() {
        return false;
    }
    let meeting: Vec<&str> = pa[..common].iter().map(String::as_str).collect();
    node(tree, &meeting).resolved_kind() == Kind::Parallel
}

fn decorate_all(
    g: &mut Gen,
    s: &mut StateDef,
    path: &str,
    root: bool,
    states: &[Info],
    tree: &StateDef,
) {
    g.decorate(s, path, states, tree, root);
    for c in &mut s.states {
        let p = if root || path.is_empty() {
            c.id.clone()
        } else {
            format!("{path}.{}", c.id)
        };
        decorate_all(g, c, &p, false, states, tree);
    }
}

/// A random valid definition.
pub fn definition(rng: &mut Rng) -> Definition {
    let max_depth = 1 + rng.below(3);
    let mut g = Gen {
        rng,
        counter: 0,
        max_depth,
    };
    let mut root = if g.rng.chance(70) {
        g.compound("root".into(), 0)
    } else {
        g.parallel("root".into(), 0)
    };
    let mut states = Vec::new();
    collect(&root, "", true, &mut states);
    let tree = root.clone();
    decorate_all(&mut g, &mut root, "", true, &states, &tree);
    let mut def = Definition::new("generated", root);
    def.context = json!({"n": 0});
    def.max_microsteps = 40;
    if g.rng.chance(50) {
        def.unhandled = Unhandled::Ignore;
    } else if g.rng.chance(50) {
        def.ignore = vec!["c".into(), "tick.*".into()];
    }
    def
}

/// The next event to try: usually one of the alphabet, sometimes a pending timer's event.
pub fn event(rng: &mut Rng, snapshot: &WireSnapshot, step: usize) -> Event {
    let timers: Vec<&String> = snapshot.timers.values().map(|t| &t.event).collect();
    let name = if !timers.is_empty() && rng.chance(25) {
        timers[rng.below(timers.len())].clone()
    } else {
        (*rng.pick(&EVENTS)).to_string()
    };
    Event {
        name,
        data: match rng.below(4) {
            0 => serde_json::Value::Null,
            1 => json!({"v": "text"}),
            _ => json!({"v": rng.below(5)}),
        },
        at: rng.chance(70).then_some(1000 * step as i64),
    }
}

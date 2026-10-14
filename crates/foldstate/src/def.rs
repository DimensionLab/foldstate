// SPDX-License-Identifier: Apache-2.0
//! The definition of a machine: plain data, serialisable, with no code in it.
//!
//! The JSON form of these types is schema `foldstate/1`
//! (`docs/schema/definition.schema.json`). The semantics are in `docs/semantics.md`.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Identifier of the definition schema this crate reads and writes.
pub const SCHEMA: &str = "foldstate/1";

fn is_false(b: &bool) -> bool {
    !*b
}

fn default_max_microsteps() -> u32 {
    1000
}

fn is_default_max_microsteps(n: &u32) -> bool {
    *n == default_max_microsteps()
}

/// A machine definition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    /// Always [`SCHEMA`].
    pub schema: String,
    /// Name of the machine. Not part of any state path.
    pub id: String,
    /// Initial context: an object, or null for an empty object.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub context: Value,
    /// What happens to an external event that enables no transition.
    #[serde(default, skip_serializing_if = "Unhandled::is_reject")]
    pub unhandled: Unhandled,
    /// Event descriptors that are ignored instead of rejected when nothing handles them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ignore: Vec<String>,
    /// Upper bound on microsteps in one macrostep; exceeding it is a livelock error.
    #[serde(
        default = "default_max_microsteps",
        skip_serializing_if = "is_default_max_microsteps"
    )]
    pub max_microsteps: u32,
    /// The root state. It is always active and its `id` is not part of state paths.
    pub root: StateDef,
}

impl Definition {
    /// A definition with the given name and root state and every option at its default.
    pub fn new(id: impl Into<String>, root: StateDef) -> Self {
        Definition {
            schema: SCHEMA.into(),
            id: id.into(),
            context: Value::Null,
            unhandled: Unhandled::Reject,
            ignore: Vec::new(),
            max_microsteps: default_max_microsteps(),
            root,
        }
    }
}

/// Policy for external events that enable no transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unhandled {
    /// The step fails with [`crate::StepError::Rejected`] and nothing changes.
    #[default]
    Reject,
    /// The event is accepted and changes nothing.
    Ignore,
}

impl Unhandled {
    fn is_reject(&self) -> bool {
        matches!(self, Unhandled::Reject)
    }
}

/// Kind of a state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// No child states.
    Atomic,
    /// Exactly one child state is active while it is active.
    Compound,
    /// Every child state (region) is active while it is active.
    Parallel,
    /// A final state of its parent.
    Final,
    /// A history pseudo-state of its parent.
    History,
}

/// One state and, recursively, its children.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StateDef {
    /// Unique among siblings; must not contain `.` or `#` and must not be empty.
    pub id: String,
    /// When absent: `compound` if the state has children, `atomic` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<Kind>,
    /// Compound states: the child entered by default. When absent, the first child.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial: Option<String>,
    /// History states: remember the whole active sub-configuration instead of the direct child.
    #[serde(default, skip_serializing_if = "is_false")]
    pub deep: bool,
    /// History states: targets used when there is no history yet. When empty, the parent's
    /// default entry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub states: Vec<StateDef>,
    /// Transitions, in priority order. A transition without `event` is eventless.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub on: Vec<TransitionDef>,
    /// Delayed transitions: sugar for a timer scheduled on entry and cancelled on exit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<AfterDef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entry: Vec<Action>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exit: Vec<Action>,
    /// Work owned by the state: started when the state is entered, stopped when it is exited.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub invoke: Vec<InvokeDef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Free-form data for tools (labels, colours, documentation). Never read by the kernel.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub meta: Value,
}

impl StateDef {
    /// An atomic state.
    pub fn atomic(id: impl Into<String>) -> Self {
        StateDef {
            id: id.into(),
            ..StateDef::default()
        }
    }

    /// A final state.
    pub fn final_state(id: impl Into<String>) -> Self {
        StateDef {
            id: id.into(),
            kind: Some(Kind::Final),
            ..StateDef::default()
        }
    }

    /// A compound state whose initial child is the first of `states`.
    pub fn compound(id: impl Into<String>, states: Vec<StateDef>) -> Self {
        StateDef {
            id: id.into(),
            kind: Some(Kind::Compound),
            states,
            ..StateDef::default()
        }
    }

    /// A parallel state whose children are its regions.
    pub fn parallel(id: impl Into<String>, states: Vec<StateDef>) -> Self {
        StateDef {
            id: id.into(),
            kind: Some(Kind::Parallel),
            states,
            ..StateDef::default()
        }
    }

    /// A history pseudo-state.
    pub fn history(id: impl Into<String>, deep: bool) -> Self {
        StateDef {
            id: id.into(),
            kind: Some(Kind::History),
            deep,
            ..StateDef::default()
        }
    }

    /// Adds a transition `event -> target` with no guard and no actions.
    pub fn on(mut self, event: impl Into<String>, target: impl Into<String>) -> Self {
        self.on.push(TransitionDef::new(event, target));
        self
    }

    /// Adds a transition.
    pub fn transition(mut self, t: TransitionDef) -> Self {
        self.on.push(t);
        self
    }

    /// Adds a tag.
    pub fn tag(mut self, tag: impl Into<String>) -> Self {
        self.tags.push(tag.into());
        self
    }

    /// Sets the initial child of a compound state.
    pub fn initial(mut self, child: impl Into<String>) -> Self {
        self.initial = Some(child.into());
        self
    }

    /// The kind after applying the default.
    pub fn resolved_kind(&self) -> Kind {
        self.kind.unwrap_or(if self.states.is_empty() {
            Kind::Atomic
        } else {
            Kind::Compound
        })
    }
}

/// A transition.
///
/// Targets are state references: `name` is a sibling of the source state, `.a.b` a descendant
/// of the source state, `#a.b` a path from the root. No target means the transition runs its
/// actions and changes no state.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransitionDef {
    /// Space-separated event descriptors. `a.b` matches `a.b` and `a.b.c`; `*` matches all.
    /// Absent: the transition is eventless and is taken as soon as its guard holds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard: Option<Guard>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub target: Vec<String>,
    /// An internal transition to descendants does not exit and re-enter its source.
    #[serde(default, skip_serializing_if = "is_false")]
    pub internal: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<Action>,
}

impl TransitionDef {
    /// `event -> target`.
    pub fn new(event: impl Into<String>, target: impl Into<String>) -> Self {
        TransitionDef {
            event: Some(event.into()),
            target: alloc::vec![target.into()],
            ..TransitionDef::default()
        }
    }

    /// An eventless transition to `target`.
    pub fn always(target: impl Into<String>) -> Self {
        TransitionDef {
            target: alloc::vec![target.into()],
            ..TransitionDef::default()
        }
    }

    /// A transition that handles `event` without changing state.
    pub fn stay(event: impl Into<String>) -> Self {
        TransitionDef {
            event: Some(event.into()),
            ..TransitionDef::default()
        }
    }

    pub fn guard(mut self, guard: Guard) -> Self {
        self.guard = Some(guard);
        self
    }

    pub fn action(mut self, action: Action) -> Self {
        self.actions.push(action);
        self
    }
}

/// A delayed transition.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AfterDef {
    pub delay_ms: u64,
    /// Timer id. When absent: `<state path>:after:<index>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guard: Option<Guard>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub target: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub internal: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<Action>,
}

/// Work owned by a state.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InvokeDef {
    /// Invocation id. When absent: `<state path>:invoke:<index>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// What to start; the host gives it meaning.
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Expr>,
}

/// A condition on a transition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Guard {
    /// The referenced state is active.
    In(String),
    Not(Box<Guard>),
    /// Every guard holds (true when empty).
    All(Vec<Guard>),
    /// Some guard holds (false when empty).
    Any(Vec<Guard>),
    Cmp {
        left: Expr,
        op: CmpOp,
        right: Expr,
    },
    /// A guard supplied by the host. It must be a pure function of its arguments.
    Named {
        name: String,
        #[serde(default, skip_serializing_if = "Value::is_null")]
        params: Value,
    },
}

/// Comparison operator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// A value computed from the context and the current event.
///
/// Paths are JSON Pointers (RFC 6901); a missing path is `null`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Expr {
    Lit(Value),
    /// A value in the context.
    Ctx(String),
    /// A value in the data of the event being processed.
    Event(String),
    Add(Box<Expr>, Box<Expr>),
    Sub(Box<Expr>, Box<Expr>),
    Mul(Box<Expr>, Box<Expr>),
}

/// Something a transition or a state does. Actions never perform I/O: they change the
/// context, queue an internal event, or produce an [`crate::Effect`] for the host.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Sets the context value at a JSON Pointer, creating missing objects on the way.
    Assign { path: String, value: Expr },
    /// Queues an internal event, processed before the next external event.
    Raise {
        event: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<Expr>,
    },
    /// Asks the host to do something.
    Emit {
        name: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        params: Option<Expr>,
    },
    /// Asks the host to deliver `event` after `delay_ms`.
    Schedule {
        id: String,
        event: String,
        delay_ms: u64,
    },
    /// Cancels a scheduled event.
    Cancel { id: String },
}

/// An event given to a machine.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub name: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
    /// Time of the event in milliseconds, on whatever clock the host uses. The kernel never
    /// reads a clock; it uses this only to compute when timers are due.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<i64>,
}

impl Event {
    pub fn new(name: impl Into<String>) -> Self {
        Event {
            name: name.into(),
            data: Value::Null,
            at: None,
        }
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = data;
        self
    }

    pub fn at(mut self, ms: i64) -> Self {
        self.at = Some(ms);
        self
    }
}

/// Something the host must do after a step. Effects are data; the kernel performs none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    Emit {
        name: String,
        #[serde(default, skip_serializing_if = "Value::is_null")]
        params: Value,
    },
    /// Deliver `event` after `delay_ms`. `due_ms` is set when the triggering event had a time.
    Schedule {
        id: String,
        event: String,
        delay_ms: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        due_ms: Option<i64>,
    },
    Cancel {
        id: String,
    },
    /// Start the work a state owns.
    Start {
        id: String,
        name: String,
        #[serde(default, skip_serializing_if = "Value::is_null")]
        params: Value,
    },
    /// Stop the work a state owned: the state was exited.
    Stop {
        id: String,
    },
}

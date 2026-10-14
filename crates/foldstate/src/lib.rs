// SPDX-License-Identifier: Apache-2.0
#![no_std]
#![doc = include_str!("../README.md")]

extern crate alloc;

pub mod canon;
pub mod def;
pub mod error;
mod inspect;
mod kernel;
mod log;
mod machine;
mod small;
mod snapshot;
pub mod value;
pub mod wire;

pub use def::{
    Action, AfterDef, CmpOp, Definition, Effect, Event, Expr, Guard, InvokeDef, Kind, SCHEMA,
    StateDef, TransitionDef, Unhandled,
};
pub use error::{DefinitionError, DefinitionErrorKind, EvalError, LogError, StepError, WireError};
pub use inspect::{
    Candidate, EnabledEvent, Explanation, GuardTrace, Outcome, StateInfo, TransitionInfo, Verdict,
};
pub use kernel::{Change, INIT_EVENT, Step};
pub use log::{BranchId, BranchInfo, Cursor, Diff, Entry, EntryKind, LOG_SCHEMA, Log, MAIN};
pub use machine::{Limits, Machine};
pub use serde_json::Value;
pub use snapshot::Snapshot;
pub use value::ValueChange;
pub use wire::{
    GuardResult, Invocation, ReplayError, SNAPSHOT_SCHEMA, Status, Timer, TransitionRef,
    WireBranch, WireEntry, WireEntryKind, WireLog, WireOp, WireParent, WireSnapshot,
};

/// Guards supplied by the host, referenced from a definition by name.
///
/// A guard must be a pure function of its arguments: the same inputs always give the same
/// answer. Results are recorded in the log, so a log can be replayed without the host.
pub trait Guards {
    /// `None` when the host has no guard of that name; the guard then counts as false.
    fn eval(&self, name: &str, params: &Value, context: &Value, event: &Event) -> Option<bool>;
}

/// A host with no guards.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoGuards;

impl Guards for NoGuards {
    fn eval(&self, _: &str, _: &Value, _: &Value, _: &Event) -> Option<bool> {
        None
    }
}

impl<F> Guards for F
where
    F: Fn(&str, &Value, &Value, &Event) -> Option<bool>,
{
    fn eval(&self, name: &str, params: &Value, context: &Value, event: &Event) -> Option<bool> {
        self(name, params, context, event)
    }
}

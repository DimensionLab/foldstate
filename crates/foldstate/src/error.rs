// SPDX-License-Identifier: Apache-2.0
//! Error types. Every error says where it happened: a place in the definition, an event, or a
//! position in the log.

use alloc::string::String;
use core::fmt;

/// Why a definition does not compile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinitionError {
    pub kind: DefinitionErrorKind,
    /// Where: a state path (`a.b`, empty for the root), followed by the field when it helps,
    /// for example `steps.train on[2].target[0]`.
    pub at: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DefinitionErrorKind {
    /// `schema` is not `foldstate/1`.
    UnknownSchema(String),
    /// A state id is empty or contains `.` or `#`.
    BadStateId(String),
    DuplicateStateId(String),
    /// A state reference that resolves to no state.
    UnknownState(String),
    /// `initial` names something that is not a child state.
    BadInitial(String),
    /// A compound or parallel state without child states.
    NoChildren,
    /// An atomic, final or history state with child states.
    UnexpectedChildren,
    /// A history state whose parent is not a compound or parallel state, or the root as one.
    BadHistory,
    /// The root must be a compound or parallel state.
    BadRoot,
    /// Targets of one transition that are not in different regions of a parallel state.
    ConflictingTargets(String, String),
    /// A transition into a history state's own descendants, or a history default that leaves
    /// the parent.
    BadHistoryDefault(String),
    /// A final state with transitions, delayed transitions or invocations.
    FinalWithTransitions,
    /// A final state whose parent is not a compound state.
    FinalNeedsCompoundParent,
    /// A transition that targets the root.
    TargetsRoot,
    /// A history state with anything but `deep` and `default`.
    HistoryWithContent,
    /// An event descriptor that is empty or malformed.
    BadEventDescriptor(String),
    /// A context path that is not a JSON Pointer.
    BadPointer(String),
    /// Two timers or two invocations with the same id.
    DuplicateId(String),
    /// `context` is neither an object nor null.
    BadContext,
    /// The definition is larger than the limits given to `compile_with`.
    TooLarge(&'static str),
}

impl fmt::Display for DefinitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use DefinitionErrorKind::*;
        let at = if self.at.is_empty() {
            "<root>"
        } else {
            &self.at
        };
        match &self.kind {
            UnknownSchema(s) => write!(f, "unknown schema `{s}`; expected `foldstate/1`"),
            BadStateId(s) => write!(f, "{at}: state id `{s}` is empty or contains `.` or `#`"),
            DuplicateStateId(s) => write!(f, "{at}: two child states are named `{s}`"),
            UnknownState(s) => write!(f, "{at}: no state `{s}`"),
            BadInitial(s) => write!(f, "{at}: initial `{s}` is not a child state"),
            NoChildren => write!(f, "{at}: a compound or parallel state needs child states"),
            UnexpectedChildren => write!(f, "{at}: this kind of state cannot have child states"),
            BadHistory => write!(
                f,
                "{at}: a history state needs a compound or parallel parent"
            ),
            BadRoot => write!(f, "the root must be a compound or parallel state"),
            ConflictingTargets(a, b) => {
                write!(
                    f,
                    "{at}: targets `{a}` and `{b}` are not in different regions of a parallel state"
                )
            }
            BadHistoryDefault(s) => {
                write!(
                    f,
                    "{at}: history default `{s}` must be a descendant of the parent state"
                )
            }
            FinalWithTransitions => write!(f, "{at}: a final state cannot have transitions"),
            FinalNeedsCompoundParent => {
                write!(
                    f,
                    "{at}: a final state must be the child of a compound state"
                )
            }
            TargetsRoot => write!(f, "{at}: a transition cannot target the root"),
            HistoryWithContent => {
                write!(f, "{at}: a history state takes only `deep` and `default`")
            }
            BadEventDescriptor(s) => write!(f, "{at}: bad event descriptor `{s}`"),
            BadPointer(s) => write!(f, "{at}: `{s}` is not a JSON Pointer"),
            DuplicateId(s) => write!(f, "{at}: id `{s}` is used twice"),
            BadContext => write!(f, "context must be an object or null"),
            TooLarge(what) => write!(f, "the definition has too many {what}"),
        }
    }
}

impl core::error::Error for DefinitionError {}

/// Why an expression or an action could not be evaluated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalError {
    BadPointer(String),
    /// A write through something that is not an object, or past the end of an array.
    NotAContainer(String),
    /// Arithmetic on a value that is not a number.
    NotANumber,
    /// Integer overflow.
    Overflow,
    /// A float result that is infinite or not a number.
    NotFinite,
    /// A named guard the host does not provide.
    UnknownGuard(String),
}

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EvalError::BadPointer(p) => write!(f, "`{p}` is not a JSON Pointer"),
            EvalError::NotAContainer(p) => {
                write!(f, "cannot write `{p}`: its parent is not an object")
            }
            EvalError::NotANumber => write!(f, "arithmetic on a value that is not a number"),
            EvalError::Overflow => write!(f, "integer overflow"),
            EvalError::NotFinite => write!(f, "a number became infinite or NaN"),
            EvalError::UnknownGuard(n) => write!(f, "no guard named `{n}` was provided"),
        }
    }
}

impl core::error::Error for EvalError {}

/// Why a step did not happen. The snapshot is unchanged in every case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepError {
    /// No transition is enabled for the event and the definition does not ignore it.
    Rejected { event: String },
    /// The machine has reached a final configuration and accepts no more events.
    Done { event: String },
    /// The macrostep did not settle within `max_microsteps`.
    Livelock { event: String, limit: u32 },
    /// A guard or an action failed.
    Eval { event: String, error: EvalError },
    /// The snapshot belongs to a different definition.
    MachineMismatch,
}

impl fmt::Display for StepError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StepError::Rejected { event } => {
                write!(f, "event `{event}` is not accepted in the current state")
            }
            StepError::Done { event } => {
                write!(f, "event `{event}` arrived after the machine finished")
            }
            StepError::Livelock { event, limit } => write!(
                f,
                "event `{event}`: the machine did not settle within {limit} microsteps"
            ),
            StepError::Eval { event, error } => write!(f, "event `{event}`: {error}"),
            StepError::MachineMismatch => {
                write!(f, "the snapshot was made by a different definition")
            }
        }
    }
}

impl core::error::Error for StepError {}

/// Why a snapshot or a log entry in its serialised form cannot be used with a machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    /// Made by a different definition.
    MachineMismatch,
    /// A format this version does not read.
    UnknownSchema(String),
    UnknownState(String),
    /// A transition reference that the definition does not contain.
    UnknownTransition(String),
    /// The active states do not form a legal configuration.
    IllegalConfiguration(String),
    BadHash(String),
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::MachineMismatch => write!(f, "made by a different definition"),
            WireError::UnknownSchema(s) => write!(f, "unknown format `{s}`"),
            WireError::UnknownState(s) => write!(f, "no state `{s}` in the definition"),
            WireError::UnknownTransition(s) => write!(f, "no transition `{s}` in the definition"),
            WireError::IllegalConfiguration(s) => write!(f, "illegal configuration: {s}"),
            WireError::BadHash(s) => write!(f, "`{s}` is not a SHA-256 hash in hex"),
        }
    }
}

impl core::error::Error for WireError {}

/// Why a log operation failed. The log is unchanged in every case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogError {
    Step(StepError),
    Wire(WireError),
    UnknownBranch(u32),
    /// The sequence number is not on the branch (before its start or after its head).
    UnknownSeq {
        branch: u32,
        seq: u64,
    },
    /// Replaying the log with this machine gives a different result at `seq`.
    Diverged {
        branch: u32,
        seq: u64,
        what: &'static str,
    },
    /// The log was written by a different definition.
    MachineMismatch,
    /// The cursor is at the first or the last entry.
    AtEnd,
    /// The serialised log is not well formed (gaps, wrong order, missing parent).
    Malformed(&'static str),
}

impl fmt::Display for LogError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogError::Step(e) => e.fmt(f),
            LogError::Wire(e) => e.fmt(f),
            LogError::UnknownBranch(b) => write!(f, "no branch {b}"),
            LogError::UnknownSeq { branch, seq } => write!(f, "no entry {seq} on branch {branch}"),
            LogError::Diverged { branch, seq, what } => write!(
                f,
                "replay diverges at entry {seq} of branch {branch}: {what} differ"
            ),
            LogError::MachineMismatch => write!(f, "the log was written by a different definition"),
            LogError::AtEnd => write!(f, "the cursor cannot move further"),
            LogError::Malformed(what) => write!(f, "malformed log: {what}"),
        }
    }
}

impl core::error::Error for LogError {}

impl From<StepError> for LogError {
    fn from(e: StepError) -> Self {
        LogError::Step(e)
    }
}

impl From<WireError> for LogError {
    fn from(e: WireError) -> Self {
        LogError::Wire(e)
    }
}

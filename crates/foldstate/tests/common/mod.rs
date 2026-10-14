// SPDX-License-Identifier: Apache-2.0
#![allow(dead_code)]

pub mod generate;
pub mod reference;

use foldstate::{Event, Value};

/// The host guards used by the tests: `even` and `odd` look at `/n` in the context; any other
/// name is unknown.
pub fn guards(name: &str, _params: &Value, context: &Value, _event: &Event) -> Option<bool> {
    let n = context.pointer("/n").and_then(Value::as_i64).unwrap_or(0);
    match name {
        "even" => Some(n % 2 == 0),
        "odd" => Some(n % 2 != 0),
        _ => None,
    }
}

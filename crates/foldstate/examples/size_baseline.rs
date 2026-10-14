// SPDX-License-Identifier: Apache-2.0
//! The part of `size_probe` that is not the engine: parse JSON and print it again.

use std::hint::black_box;

fn main() {
    let text = black_box(String::from(r#"{"schema":"foldstate/1","id":"m"}"#));
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else {
        return;
    };
    black_box(serde_json::to_string(&value).unwrap_or_default());
}

// SPDX-License-Identifier: Apache-2.0
//! The smallest useful consumer: read a serialised log and follow it entry by entry,
//! forwards and backwards, without the definition. Built for `wasm32-unknown-unknown` next
//! to `size_baseline`, it shows what a viewer that only scrubs a timeline pays.

use std::hint::black_box;

use foldstate::{WireLog, WireSnapshot};

fn main() {
    let text = black_box(String::from(
        r#"{"schema":"foldstate-log/1","machine":"00","branches":[{"id":0,"entries":[{"seq":1,"kind":{"start":{}},"ops":[{"enter":"a"}]}]}]}"#,
    ));
    let Ok(log) = serde_json::from_str::<WireLog>(&text) else {
        return;
    };
    let mut snapshot = WireSnapshot::blank(log.machine.clone());
    for branch in &log.branches {
        for entry in &branch.entries {
            let _ = snapshot.redo(entry);
        }
        for entry in branch.entries.iter().rev() {
            let _ = snapshot.undo(entry);
        }
    }
    black_box(snapshot.configuration.len());
}

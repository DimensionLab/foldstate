// SPDX-License-Identifier: Apache-2.0
//! Helpers over [`serde_json::Value`]: JSON Pointer reads and writes, comparison, arithmetic
//! and structural difference.

use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cmp::Ordering;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};

use crate::error::EvalError;

/// Splits a JSON Pointer into unescaped segments. `""` is the whole document.
pub(crate) fn segments(ptr: &str) -> Result<Vec<String>, EvalError> {
    if ptr.is_empty() {
        return Ok(Vec::new());
    }
    let Some(rest) = ptr.strip_prefix('/') else {
        return Err(EvalError::BadPointer(ptr.to_string()));
    };
    Ok(rest
        .split('/')
        .map(|s| s.replace("~1", "/").replace("~0", "~"))
        .collect())
}

fn escape(seg: &str) -> String {
    seg.replace('~', "~0").replace('/', "~1")
}

/// The value at `ptr`, if present.
pub fn get<'a>(root: &'a Value, ptr: &str) -> Option<&'a Value> {
    root.pointer(ptr)
}

/// A write expressed so that it can be undone: `path` exists in the document or its parent
/// does, `before` is what was there (`None`: nothing) and `after` what will be.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SetPlan {
    pub path: String,
    pub before: Option<Value>,
    pub after: Value,
}

/// Plans `root[ptr] = new`, creating missing objects on the way.
pub(crate) fn plan_set(root: &Value, ptr: &str, new: Value) -> Result<SetPlan, EvalError> {
    // The usual case, without allocation: no escapes, and the value or at least its parent
    // object exists.
    if let Some((parent, leaf)) = ptr.rsplit_once('/')
        && !ptr.contains('~')
        && let Some(Value::Object(m)) = root.pointer(parent)
    {
        return Ok(SetPlan {
            path: ptr.to_string(),
            before: m.get(leaf).cloned(),
            after: new,
        });
    }
    let segs = segments(ptr)?;
    let mut cur = root;
    let mut path = String::new();
    for (i, seg) in segs.iter().enumerate() {
        let last = i + 1 == segs.len();
        let next = match cur {
            Value::Object(m) => m.get(seg.as_str()),
            Value::Array(a) => match seg.parse::<usize>().ok().and_then(|n| a.get(n)) {
                Some(v) => Some(v),
                None => return Err(EvalError::NotAContainer(ptr.to_string())),
            },
            _ => return Err(EvalError::NotAContainer(ptr.to_string())),
        };
        path.push('/');
        path.push_str(&escape(seg));
        match next {
            Some(v) if last => {
                return Ok(SetPlan {
                    path,
                    before: Some(v.clone()),
                    after: new,
                });
            }
            Some(v) => cur = v,
            None => {
                // Wrap the value in the objects that are missing below this point.
                let mut after = new;
                for s in segs[i + 1..].iter().rev() {
                    let mut m = Map::new();
                    m.insert(s.clone(), after);
                    after = Value::Object(m);
                }
                return Ok(SetPlan {
                    path,
                    before: None,
                    after,
                });
            }
        }
    }
    Ok(SetPlan {
        path,
        before: Some(root.clone()),
        after: new,
    })
}

/// Sets (`Some`) or removes (`None`) the value at `ptr`, whose parent must exist.
pub(crate) fn put(root: &mut Value, ptr: &str, value: Option<Value>) -> Result<(), EvalError> {
    if let Some((parent, leaf)) = ptr.rsplit_once('/')
        && !ptr.contains('~')
        && let Some(Value::Object(m)) = root.pointer_mut(parent)
    {
        match value {
            Some(v) => match m.get_mut(leaf) {
                Some(slot) => *slot = v,
                None => {
                    m.insert(leaf.to_string(), v);
                }
            },
            None => {
                m.remove(leaf);
            }
        }
        return Ok(());
    }
    let mut segs = segments(ptr)?;
    let Some(leaf) = segs.pop() else {
        *root = value.unwrap_or(Value::Null);
        return Ok(());
    };
    let mut cur = root;
    for seg in &segs {
        cur = match cur {
            Value::Object(m) => m.get_mut(seg.as_str()),
            Value::Array(a) => seg.parse::<usize>().ok().and_then(|n| a.get_mut(n)),
            _ => None,
        }
        .ok_or_else(|| EvalError::NotAContainer(ptr.to_string()))?;
    }
    match (cur, value) {
        (Value::Object(m), Some(v)) => {
            m.insert(leaf, v);
        }
        (Value::Object(m), None) => {
            m.remove(leaf.as_str());
        }
        (Value::Array(a), Some(v)) => {
            let slot = leaf
                .parse::<usize>()
                .ok()
                .and_then(|n| a.get_mut(n))
                .ok_or_else(|| EvalError::NotAContainer(ptr.to_string()))?;
            *slot = v;
        }
        _ => return Err(EvalError::NotAContainer(ptr.to_string())),
    }
    Ok(())
}

enum Num {
    Int(i64),
    Float(f64),
}

fn num(n: &Number) -> Num {
    match n.as_i64() {
        Some(i) => Num::Int(i),
        None => Num::Float(n.as_f64().unwrap_or(0.0)),
    }
}

/// Orders two values when they are comparable: two numbers (by value, so `1` equals `1.0`),
/// two strings or two booleans.
pub fn compare(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (num(x), num(y)) {
            (Num::Int(x), Num::Int(y)) => Some(x.cmp(&y)),
            (Num::Int(x), Num::Float(y)) => (x as f64).partial_cmp(&y),
            (Num::Float(x), Num::Int(y)) => x.partial_cmp(&(y as f64)),
            (Num::Float(x), Num::Float(y)) => x.partial_cmp(&y),
        },
        (Value::String(x), Value::String(y)) => Some(x.cmp(y)),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

/// Equality used by guards: numbers by value, everything else structurally.
pub fn equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(_), Value::Number(_)) => compare(a, b) == Some(Ordering::Equal),
        _ => a == b,
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Arith {
    Add,
    Sub,
    Mul,
}

/// Integer arithmetic is checked; anything involving a float is done in `f64` and must stay
/// finite.
pub(crate) fn arith(op: Arith, a: &Value, b: &Value) -> Result<Value, EvalError> {
    let (Value::Number(x), Value::Number(y)) = (a, b) else {
        return Err(EvalError::NotANumber);
    };
    match (num(x), num(y)) {
        (Num::Int(x), Num::Int(y)) => match op {
            Arith::Add => x.checked_add(y),
            Arith::Sub => x.checked_sub(y),
            Arith::Mul => x.checked_mul(y),
        }
        .map(Value::from)
        .ok_or(EvalError::Overflow),
        (x, y) => {
            let f = |n| match n {
                Num::Int(i) => i as f64,
                Num::Float(f) => f,
            };
            let (x, y) = (f(x), f(y));
            let r = match op {
                Arith::Add => x + y,
                Arith::Sub => x - y,
                Arith::Mul => x * y,
            };
            Number::from_f64(r)
                .map(Value::Number)
                .ok_or(EvalError::NotFinite)
        }
    }
}

/// Deserialises a field that is present, keeping an explicit `null` as a value: for these
/// fields "absent" and "null" mean different things.
pub(crate) fn present<'de, D>(d: D) -> Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Value::deserialize(d).map(Some)
}

/// One difference between two documents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ValueChange {
    /// JSON Pointer of the value that differs.
    pub path: String,
    /// The value on the first side; absent when the path exists only on the second.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub before: Option<Value>,
    /// The value on the second side; absent when the path exists only on the first.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub after: Option<Value>,
}

/// The differences between `a` and `b`, descending into objects and reporting everything
/// else (arrays included) as a whole.
pub fn diff(a: &Value, b: &Value) -> Vec<ValueChange> {
    let mut out = Vec::new();
    diff_into(a, b, &mut String::new(), &mut out);
    out
}

fn diff_into(a: &Value, b: &Value, path: &mut String, out: &mut Vec<ValueChange>) {
    if a == b {
        return;
    }
    if let (Value::Object(x), Value::Object(y)) = (a, b) {
        let mut keys: Vec<&String> = x.keys().chain(y.keys()).collect();
        keys.sort();
        keys.dedup();
        for k in keys {
            let len = path.len();
            path.push('/');
            path.push_str(&escape(k));
            match (x.get(k), y.get(k)) {
                (Some(va), Some(vb)) => diff_into(va, vb, path, out),
                (va, vb) => out.push(ValueChange {
                    path: path.clone(),
                    before: va.cloned(),
                    after: vb.cloned(),
                }),
            }
            path.truncate(len);
        }
        return;
    }
    out.push(ValueChange {
        path: path.clone(),
        before: Some(a.clone()),
        after: Some(b.clone()),
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn set_creates_missing_objects_and_is_undoable() {
        let mut doc = json!({"a": {"x": 1}});
        let plan = plan_set(&doc, "/a/b/c", json!(5)).unwrap();
        assert_eq!(plan.path, "/a/b");
        assert_eq!(plan.before, None);
        let before = doc.clone();
        put(&mut doc, &plan.path, Some(plan.after.clone())).unwrap();
        assert_eq!(doc, json!({"a": {"x": 1, "b": {"c": 5}}}));
        put(&mut doc, &plan.path, plan.before.clone()).unwrap();
        assert_eq!(doc, before);
    }

    #[test]
    fn set_replaces_existing_and_array_elements() {
        let doc = json!({"a": [1, 2], "k/1": 0});
        let plan = plan_set(&doc, "/a/1", json!(9)).unwrap();
        assert_eq!((plan.path.as_str(), plan.before), ("/a/1", Some(json!(2))));
        assert!(plan_set(&doc, "/a/7", json!(9)).is_err());
        assert!(plan_set(&doc, "/a/0/x", json!(9)).is_err());
        assert_eq!(plan_set(&doc, "/k~11", json!(1)).unwrap().path, "/k~11");
        assert!(plan_set(&doc, "nope", json!(1)).is_err());
    }

    #[test]
    fn numbers_compare_by_value() {
        assert!(equal(&json!(1), &json!(1.0)));
        assert_eq!(compare(&json!(2), &json!(2.5)), Some(Ordering::Less));
        assert_eq!(compare(&json!("a"), &json!(1)), None);
        assert_eq!(
            arith(Arith::Add, &json!(i64::MAX), &json!(1)),
            Err(EvalError::Overflow)
        );
        assert_eq!(
            arith(Arith::Mul, &json!(2), &json!(1.5)).unwrap(),
            json!(3.0)
        );
        assert_eq!(
            arith(Arith::Mul, &json!(1e308), &json!(1e10)),
            Err(EvalError::NotFinite)
        );
    }

    #[test]
    fn diff_descends_into_objects() {
        let d = diff(
            &json!({"a": {"b": 1, "c": 2}, "x": [1]}),
            &json!({"a": {"b": 1, "c": 3}, "y": 0}),
        );
        let paths: Vec<&str> = d.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["/a/c", "/x", "/y"]);
    }
}

// Copyright (c) 2026 Just a Nerd
// SPDX-License-Identifier: BUSL-1.1

//! Composing contributions into one state.
//!
//! Every plugin keeps its own opinion of a subject. The subject's state is
//! the merge of those opinions in admission order: objects merge key by
//! key, a later plugin's key over an earlier one's; anything that is not
//! an object is taken whole from the last plugin to offer one.

use serde_json::Value;

/// Merge `contributions`, oldest first, into one value.
pub fn compose<'a>(contributions: impl Iterator<Item = &'a Value>) -> Value {
    let mut out = Value::Null;
    for c in contributions {
        merge_into(&mut out, c);
    }
    out
}

fn merge_into(target: &mut Value, next: &Value) {
    match (target, next) {
        (Value::Object(t), Value::Object(n)) => {
            for (k, v) in n {
                match t.get_mut(k) {
                    Some(existing) if existing.is_object() && v.is_object() => {
                        merge_into(existing, v)
                    }
                    _ => {
                        t.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (t, n) => *t = n.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn objects_merge_later_keys_winning() {
        let a = json!({"title": "A", "artist": "X", "art": {"url": "u1"}});
        let b = json!({"title": "B", "art": {"w": 300}});
        let out = compose([&a, &b].into_iter());
        assert_eq!(
            out,
            json!({"title": "B", "artist": "X", "art": {"url": "u1", "w": 300}})
        );
    }

    #[test]
    fn a_non_object_replaces_whole() {
        let a = json!({"k": 1});
        let b = json!(7);
        let c = json!({"j": 2});
        assert_eq!(compose([&a, &b].into_iter()), json!(7));
        assert_eq!(compose([&a, &b, &c].into_iter()), json!({"j": 2}));
    }

    #[test]
    fn nothing_composes_to_null() {
        assert_eq!(compose(core::iter::empty()), Value::Null);
    }
}

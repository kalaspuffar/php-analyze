//! Function-dictionary interner for the recorder.
//!
//! Each PHP function observed during a trace gets a monotonic `fn_id`
//! the first time it is seen. Subsequent observations of the same
//! function reuse that `fn_id`. The strings describing the function
//! (its fully-qualified name, declaring file, declaring line) are
//! captured once into a `DictEntry`, staged for the next batch handoff,
//! and never copied again for the lifetime of the trace.
//!
//! Design notes (`design.md §D-3 / §D-4`):
//!
//! - The `intern` method takes a `FnOnce(u32) -> DictEntry` closure so
//!   the caller pays the `DictEntry` allocation **only** on the first
//!   sight of a key. The closure receives the assigned `fn_id` so the
//!   caller can populate `DictEntry::fn_id` directly.
//! - The internal hashmap uses `rustc_hash::FxHashMap` — fast on small
//!   composite keys, zero transitive deps, already in the workspace
//!   graph via `ext-php-rs-bindgen`'s build-deps.

use rustc_hash::FxHashMap;

use crate::recorder::types::{DictEntry, FunctionKey};

/// Interns `FunctionKey`s to monotonic `fn_id`s and stages `DictEntry`s
/// for the next batch flush.
#[derive(Debug)]
pub struct Dictionary {
    intern: FxHashMap<FunctionKey, u32>,
    new_entries: Vec<DictEntry>,
    next_fn_id: u32,
}

impl Dictionary {
    /// Fresh, empty interner. Next assigned `fn_id` is `1` (matches
    /// `SPECIFICATION.md` §4.1.2 — call IDs and fn IDs both start at 1,
    /// with `0` reserved as the "no parent" / "no function" sentinel).
    pub fn new() -> Self {
        Self {
            intern: FxHashMap::default(),
            new_entries: Vec::new(),
            next_fn_id: 1,
        }
    }

    /// Look up `key`. On hit, return the existing `fn_id` without
    /// invoking `build`. On miss, allocate a fresh `fn_id`, call
    /// `build(fn_id)` exactly once, stage the resulting `DictEntry` for
    /// the next flush, and return the new `fn_id`.
    pub fn intern(&mut self, key: FunctionKey, build: impl FnOnce(u32) -> DictEntry) -> u32 {
        // `entry().or_insert_with(...)` would force us to construct the
        // `DictEntry` inside the closure, which is fine — but we also
        // need to push it onto `new_entries`, and the `entry` API does
        // not give us back a clean signal of "this was a miss" without
        // matching on the `Entry` variant or comparing the post-insert
        // length. The explicit `match get` below reads more obviously
        // as "hit vs miss" at the call site.
        if let Some(&existing) = self.intern.get(&key) {
            return existing;
        }

        let fn_id = self.next_fn_id;
        // `checked_add` rather than `+= 1`: a `+=` would panic in debug
        // and **silently wrap to 0** in release, breaking the "0 is the
        // no-function sentinel" contract documented on `new()`. Four
        // billion distinct functions in one trace is unreachable for any
        // realistic PHP workload, but the invariant should not depend on
        // workload — and the `expect` is one branch on a path that
        // already does a hashmap insert, so the cost is invisible.
        self.next_fn_id = self
            .next_fn_id
            .checked_add(1)
            .expect("fn_id counter overflowed u32 — 2^32 distinct functions in a single trace");
        self.intern.insert(key, fn_id);
        self.new_entries.push(build(fn_id));
        fn_id
    }

    /// Drain and return all `DictEntry`s staged since the last call.
    /// The interning map is left intact, so repeat lookups continue to
    /// return the same `fn_id`s without staging fresh entries.
    pub fn take_new_entries(&mut self) -> Vec<DictEntry> {
        std::mem::take(&mut self.new_entries)
    }
}

impl Default for Dictionary {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recorder::types::FunctionKind;
    use std::sync::Arc;

    fn internal_key(name: &str) -> FunctionKey {
        FunctionKey::Internal {
            name: Arc::from(name),
        }
    }

    fn internal_entry(fn_id: u32, name: &str) -> DictEntry {
        DictEntry {
            fn_id,
            fqn: format!("internal:{name}"),
            file: String::new(),
            line: 0,
            kind: FunctionKind::Internal,
        }
    }

    #[test]
    fn interning_a_new_key_allocates_a_fresh_fn_id_and_stages_an_entry() {
        let mut dict = Dictionary::new();
        let id = dict.intern(internal_key("strlen"), |fn_id| {
            internal_entry(fn_id, "strlen")
        });
        assert_eq!(id, 1);

        let staged = dict.take_new_entries();
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].fn_id, 1);
        assert_eq!(staged[0].fqn, "internal:strlen");
    }

    #[test]
    fn interning_a_repeat_key_returns_the_existing_fn_id_without_invoking_build() {
        let mut dict = Dictionary::new();
        let first = dict.intern(internal_key("strlen"), |fn_id| {
            internal_entry(fn_id, "strlen")
        });

        // Use a captured counter to assert the build closure does NOT
        // run on a hit. A closure that mutates a captured variable is
        // the standard idiom for this kind of assertion.
        let mut build_invocations = 0_u32;
        let second = dict.intern(internal_key("strlen"), |fn_id| {
            build_invocations += 1;
            internal_entry(fn_id, "strlen")
        });

        assert_eq!(first, second, "repeat key must return the same fn_id");
        assert_eq!(build_invocations, 0, "build closure must not run on a hit");
    }

    #[test]
    fn take_new_entries_drains_the_staging_buffer_but_keeps_the_interning_map() {
        let mut dict = Dictionary::new();
        let names = ["strlen", "array_map", "json_encode"];
        let first_round: Vec<u32> = names
            .iter()
            .map(|n| dict.intern(internal_key(n), |fn_id| internal_entry(fn_id, n)))
            .collect();

        // First drain: must yield exactly three staged entries.
        let staged = dict.take_new_entries();
        assert_eq!(staged.len(), 3);

        // Re-intern the same keys. The interning map is intact, so
        // every lookup is a hit; no new entries are staged.
        let second_round: Vec<u32> = names
            .iter()
            .map(|n| dict.intern(internal_key(n), |fn_id| internal_entry(fn_id, n)))
            .collect();

        assert_eq!(
            first_round, second_round,
            "fn_ids must be stable across drains"
        );

        // Second drain: nothing staged since the first drain.
        let staged_again = dict.take_new_entries();
        assert!(staged_again.is_empty(), "second drain must be empty");
    }

    #[test]
    fn fn_ids_are_monotonic_from_one_across_one_hundred_distinct_keys() {
        let mut dict = Dictionary::new();
        let ids: Vec<u32> = (0..100)
            .map(|i| {
                let name = format!("fn_{i}");
                dict.intern(internal_key(&name), |fn_id| internal_entry(fn_id, &name))
            })
            .collect();

        let expected: Vec<u32> = (1..=100).collect();
        assert_eq!(ids, expected);
    }
}

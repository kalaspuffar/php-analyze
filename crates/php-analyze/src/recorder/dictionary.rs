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
//! - The internal hashmap is `hashbrown::HashMap<_, _, FxBuildHasher>`
//!   so the recorder hot path can use `raw_entry_mut().from_hash(...)`
//!   to probe by a borrowed [`FunctionKeyRef`] without allocating an
//!   owning [`FunctionKey`] on the hit path. `hashbrown`'s `raw_entry`
//!   is the only stable-Rust escape hatch from the `K: Borrow<Q>`
//!   constraint that blocks this pattern on `std::collections::HashMap`
//!   (recorder-hot-path-tuning D-1).

use std::hash::BuildHasher;

use hashbrown::hash_map::RawEntryMut;
use hashbrown::HashMap;
use rustc_hash::FxBuildHasher;

use crate::recorder::types::{DictEntry, FunctionKey, FunctionKeyRef};

/// Interns `FunctionKey`s to monotonic `fn_id`s and stages `DictEntry`s
/// for the next batch flush.
#[derive(Debug)]
pub struct Dictionary {
    intern: HashMap<FunctionKey, u32, FxBuildHasher>,
    new_entries: Vec<DictEntry>,
    next_fn_id: u32,
}

impl Dictionary {
    /// Fresh, empty interner. Next assigned `fn_id` is `1` (matches
    /// `SPECIFICATION.md` §4.1.2 — call IDs and fn IDs both start at 1,
    /// with `0` reserved as the "no parent" / "no function" sentinel).
    pub fn new() -> Self {
        Self {
            intern: HashMap::with_hasher(FxBuildHasher),
            new_entries: Vec::new(),
            next_fn_id: 1,
        }
    }

    /// Look up `key`. On hit, return the existing `fn_id` without
    /// invoking `build`. On miss, allocate a fresh `fn_id`, call
    /// `build(fn_id)` exactly once, stage the resulting `DictEntry` for
    /// the next flush, and return the new `fn_id`.
    ///
    /// Use [`Dictionary::intern_ref`] on the recorder hot path —
    /// `intern` materialises an owning `FunctionKey` up-front and so
    /// allocates its `Arc<str>` fields even on hits. This method
    /// stays for the call sites (tests, slice-2 paths) that already
    /// hold an owning key.
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

        let fn_id = self.allocate_fn_id();
        self.intern.insert(key, fn_id);
        self.new_entries.push(build(fn_id));
        fn_id
    }

    /// Borrow-keyed lookup-or-intern entry point. Probes the intern
    /// map by [`FunctionKeyRef`] (no `Arc<str>` allocation); on a miss,
    /// invokes `build` exactly once to obtain the owning
    /// `(FunctionKey, DictEntry)` pair, inserts both, and returns the
    /// fresh `fn_id`.
    ///
    /// On a hit returns `(fn_id, Hit)`; on a miss returns
    /// `(fn_id, Miss)`. The hit/miss signal lets the caller's
    /// cap-gate projection (`dict_miss_cost`) read the would-be
    /// allocation cost in the same hashmap traversal that the commit
    /// uses — there is no need for a separate `contains_key` probe.
    ///
    /// **Zero-alloc contract**: on the hit path this method performs
    /// no heap allocation (the only operations are the precomputed
    /// hash, the bucket walk, and a `u32` return). Verified by the
    /// `recorder-zero-alloc-audit` harness for AC-RC-5.
    pub fn intern_ref<F>(&mut self, key_ref: &FunctionKeyRef<'_>, build: F) -> (u32, ProbeOutcome)
    where
        F: FnOnce(u32) -> (FunctionKey, DictEntry),
    {
        let hash = FxBuildHasher.hash_one(key_ref);
        let entry = self
            .intern
            .raw_entry_mut()
            .from_hash(hash, |k| k.matches_ref(key_ref));

        match entry {
            RawEntryMut::Occupied(o) => (*o.get(), ProbeOutcome::Hit),
            RawEntryMut::Vacant(v) => {
                let fn_id = self.next_fn_id;
                // `checked_add` rather than `+= 1`: a `+=` would panic
                // in debug and **silently wrap to 0** in release,
                // breaking the "0 is the no-function sentinel"
                // contract documented on `new()`.
                self.next_fn_id = self.next_fn_id.checked_add(1).expect(
                    "fn_id counter overflowed u32 — 2^32 distinct functions in a single trace",
                );
                let (owning_key, entry) = build(fn_id);
                v.insert_hashed_nocheck(hash, owning_key, fn_id);
                self.new_entries.push(entry);
                (fn_id, ProbeOutcome::Miss)
            }
        }
    }

    /// Return whether `key_ref` is currently interned, without
    /// staging anything. Sibling of [`Dictionary::contains_key`] for
    /// borrowed views; used by code paths that need to read the
    /// miss-cost projection independently of the commit.
    ///
    /// **Zero-alloc**: no `Arc<str>` allocation on either branch.
    pub fn contains_key_ref(&self, key_ref: &FunctionKeyRef<'_>) -> bool {
        let hash = FxBuildHasher.hash_one(key_ref);
        self.intern
            .raw_entry()
            .from_hash(hash, |k| k.matches_ref(key_ref))
            .is_some()
    }

    /// Allocate the next `fn_id` and advance the counter. Shared
    /// between `intern` and `intern_ref`.
    fn allocate_fn_id(&mut self) -> u32 {
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
        fn_id
    }

    /// `true` if `key` is already interned. Cheap — a single hashmap
    /// probe — and added in slice 3 (`recorder-depth-and-cap-drops`)
    /// so the cap-gate can project whether a begin would incur a
    /// dictionary-miss allocation without itself triggering the
    /// allocation. The cap-gate uses this read **before** staging so
    /// a dropped begin never leaves a half-interned key behind.
    pub fn contains_key(&self, key: &FunctionKey) -> bool {
        self.intern.contains_key(key)
    }

    /// Drain and return all `DictEntry`s staged since the last call.
    /// The interning map is left intact, so repeat lookups continue to
    /// return the same `fn_id`s without staging fresh entries.
    pub fn take_new_entries(&mut self) -> Vec<DictEntry> {
        std::mem::take(&mut self.new_entries)
    }

    /// Borrow the staged entries without draining them. Used **only**
    /// by `recorder::dump` (diagnostic-only, behind the
    /// `recorder-dump` Cargo feature) so the slice-2 integration
    /// tests can inspect the dictionary contents before the trace is
    /// dropped. Production code MUST go through
    /// [`take_new_entries`](Self::take_new_entries), which transfers
    /// ownership to the future shipper batch.
    #[cfg(feature = "recorder-dump")]
    pub(crate) fn new_entries_for_dump(&self) -> &[DictEntry] {
        &self.new_entries
    }
}

impl Default for Dictionary {
    fn default() -> Self {
        Self::new()
    }
}

/// Signal returned by [`Dictionary::intern_ref`] so the caller can tell
/// hit from miss in the same hashmap traversal that the commit uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeOutcome {
    Hit,
    Miss,
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

    // --- intern_ref (borrow-keyed probe) ----------------------------------

    #[test]
    fn intern_ref_on_a_miss_invokes_build_exactly_once_and_returns_miss_outcome() {
        let mut dict = Dictionary::new();
        let mut invocations = 0_u32;
        let key_ref = FunctionKeyRef::Internal { name: "strlen" };
        let (fn_id, outcome) = dict.intern_ref(&key_ref, |fn_id| {
            invocations += 1;
            (
                FunctionKey::Internal {
                    name: Arc::from("strlen"),
                },
                internal_entry(fn_id, "strlen"),
            )
        });
        assert_eq!(fn_id, 1);
        assert_eq!(outcome, ProbeOutcome::Miss);
        assert_eq!(invocations, 1);

        let staged = dict.take_new_entries();
        assert_eq!(staged.len(), 1);
        assert_eq!(staged[0].fn_id, 1);
    }

    #[test]
    fn intern_ref_on_a_hit_returns_existing_fn_id_without_invoking_build() {
        let mut dict = Dictionary::new();
        let _first = dict.intern_ref(&FunctionKeyRef::Internal { name: "strlen" }, |fn_id| {
            (
                FunctionKey::Internal {
                    name: Arc::from("strlen"),
                },
                internal_entry(fn_id, "strlen"),
            )
        });

        // Use a panicking build closure to prove it is never invoked on a
        // hit. Captured-counter would also work, but the panicking
        // closure makes a misfire impossible to miss.
        let (fn_id, outcome) =
            dict.intern_ref(&FunctionKeyRef::Internal { name: "strlen" }, |_| {
                panic!("build closure must not run on a dict hit");
            });
        assert_eq!(fn_id, 1);
        assert_eq!(outcome, ProbeOutcome::Hit);
    }

    #[test]
    fn intern_ref_finds_entries_inserted_via_the_owning_intern_api() {
        let mut dict = Dictionary::new();
        let owning_id = dict.intern(internal_key("array_map"), |fn_id| {
            internal_entry(fn_id, "array_map")
        });

        let (borrow_id, outcome) =
            dict.intern_ref(&FunctionKeyRef::Internal { name: "array_map" }, |_| {
                panic!("must hit the entry inserted via the owning API");
            });
        assert_eq!(borrow_id, owning_id);
        assert_eq!(outcome, ProbeOutcome::Hit);
    }

    #[test]
    fn intern_ref_distinguishes_variants_with_the_same_inner_string() {
        let mut dict = Dictionary::new();
        let internal_id = dict
            .intern_ref(&FunctionKeyRef::Internal { name: "noop" }, |fn_id| {
                (
                    FunctionKey::Internal {
                        name: Arc::from("noop"),
                    },
                    internal_entry(fn_id, "noop"),
                )
            })
            .0;
        let function_id = dict
            .intern_ref(
                &FunctionKeyRef::Function {
                    file: "",
                    function: "noop",
                    line: 0,
                },
                |fn_id| {
                    (
                        FunctionKey::Function {
                            file: Arc::from(""),
                            function: Arc::from("noop"),
                            line: 0,
                        },
                        DictEntry {
                            fn_id,
                            fqn: "noop".to_owned(),
                            file: String::new(),
                            line: 0,
                            kind: FunctionKind::Function,
                        },
                    )
                },
            )
            .0;

        assert_ne!(
            internal_id, function_id,
            "Internal{{name}} and Function{{function,...}} must not collide on the same string"
        );
    }

    #[test]
    fn contains_key_ref_returns_true_after_an_owning_intern() {
        let mut dict = Dictionary::new();
        dict.intern(internal_key("strlen"), |fn_id| {
            internal_entry(fn_id, "strlen")
        });
        assert!(dict.contains_key_ref(&FunctionKeyRef::Internal { name: "strlen" }));
        assert!(!dict.contains_key_ref(&FunctionKeyRef::Internal {
            name: "json_encode"
        }));
    }
}

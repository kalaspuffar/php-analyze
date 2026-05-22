//! Per-call recorder hot-path micro-benchmark.
//!
//! Times **one** `begin_with_snapshots + end_with_snapshots` pair
//! against a pre-allocated `Trace` per criterion iteration. The
//! criterion-reported "time per iteration" is the per-call cost in
//! nanoseconds — no PHP, no observer trampoline, no encoding, no
//! HTTP. This is the bench that pins NFR-PERF-1's per-call
//! component for the recorder kernel.
//!
//! No pass criterion is asserted in this slice; the threshold
//! (geo-mean ≤ 2.0×, per-call budget) is the job of
//! `bench-canonical-workloads`. This bench's job is to *exist* so
//! drift is measurable.
//!
//! Run with:
//!
//! ```sh
//! cargo bench -p php-analyze --features bench-seam --bench recorder_hot_path
//! ```

#[cfg(not(feature = "bench-seam"))]
compile_error!(
    "crates/php-analyze/benches/recorder_hot_path.rs requires the bench-seam feature: \
     run `cargo bench -p php-analyze --features bench-seam --bench recorder_hot_path` \
     (the feature gates the `php_analyze::bench_seam` module that this bench imports)"
);

#[cfg(feature = "bench-seam")]
use std::borrow::Cow;
#[cfg(feature = "bench-seam")]
use std::sync::Arc;

#[cfg(feature = "bench-seam")]
use criterion::{criterion_group, criterion_main, Criterion};
#[cfg(feature = "bench-seam")]
use php_analyze::bench_seam::{
    begin_with_snapshots, end_with_snapshots, Categorised, EntrySnapshots, ExitSnapshots,
    FunctionKey, FunctionKind, RequestIdentity, Trace, TraceLimits,
};

/// Construct a `Trace` whose limits are large enough to prevent any
/// flush or cap-drop from firing during the bench. Buffer grows
/// monotonically across criterion iterations (criterion runs the
/// inner closure many thousand times to measure stable per-iteration
/// timing), and `flush_records = usize::MAX` keeps the flush gate
/// quiet for that entire window.
#[cfg(feature = "bench-seam")]
fn make_trace() -> Trace {
    let identity = RequestIdentity {
        host: Arc::from("bench-host"),
        sapi: Arc::from("cli"),
        pid: 0,
        uri_or_script: Arc::from("/bench/recorder_hot_path.rs"),
    };
    let limits = TraceLimits {
        flush_records: usize::MAX,
        flush_bytes: usize::MAX,
        buffer_cap_bytes: usize::MAX,
        max_depth: 1024,
    };
    Trace::new(identity, limits)
}

/// A single fixed `Categorised` value representing a `noop`-shaped
/// user function. Constructed once outside the timed region.
///
/// `'static` lifetime: the `Cow`s are `Cow::Borrowed(&'static str)`,
/// so the value's interior references are static-program-data and
/// the value itself can sit in a local for the bench's lifetime
/// without borrow-checker contortions.
#[cfg(feature = "bench-seam")]
fn make_categorised() -> Categorised<'static> {
    Categorised {
        key: FunctionKey::Function {
            file: Arc::from("/bench/fixture.php"),
            function: Arc::from("noop"),
            line: 1,
        },
        kind: FunctionKind::Function,
        fqn: Cow::Borrowed("noop"),
        file: Cow::Borrowed("/bench/fixture.php"),
        line: 1,
    }
}

/// Pre-captured zero snapshots — they exercise the hot-path
/// arithmetic without taking real clock/memory readings (those
/// reads happen in `Recorder::begin`/`end`, which is the
/// `FcallObserver` trampoline above `begin_with_snapshots`, not
/// the entry function we're timing).
#[cfg(feature = "bench-seam")]
const ENTRY_SNAPSHOTS: EntrySnapshots = EntrySnapshots {
    t_in_ns: 0,
    cpu_u_in_ns: 0,
    cpu_s_in_ns: 0,
    mem_in_bytes: 0,
};

#[cfg(feature = "bench-seam")]
const EXIT_SNAPSHOTS: ExitSnapshots = ExitSnapshots {
    t_out_ns: 0,
    cpu_u_now_ns: 0,
    cpu_s_now_ns: 0,
    mem_out_bytes: 0,
};

#[cfg(feature = "bench-seam")]
fn bench_hot_path(c: &mut Criterion) {
    // The trace lives across iterations. Criterion runs the closure
    // many times per measurement; `flush_records = usize::MAX` and
    // `buffer_cap_bytes = usize::MAX` keep the buffer growing
    // without tripping flush or cap-drop. The trace is therefore
    // **not** representative of a steady-state trace's
    // dictionary-hit ratio across calls — but for the per-call cost
    // measurement the dictionary is already warm (the first
    // iteration's miss is amortised across millions of subsequent
    // hits).
    let mut trace = make_trace();
    let categorised = make_categorised();

    c.bench_function("recorder_hot_path", |b| {
        b.iter(|| {
            begin_with_snapshots(&mut trace, &categorised, ENTRY_SNAPSHOTS);
            end_with_snapshots(&mut trace, EXIT_SNAPSHOTS, false);
        });
    });
}

#[cfg(feature = "bench-seam")]
criterion_group!(benches, bench_hot_path);
#[cfg(feature = "bench-seam")]
criterion_main!(benches);

// `compile_error!` above guarantees the bench file only compiles
// with the bench-seam feature; under `cargo build` without the
// feature this file is empty after the `#[cfg]` guard, which is
// valid for a bench target.

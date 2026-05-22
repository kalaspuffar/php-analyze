//! Recorder workload-shape benchmark.
//!
//! Runs **10_000 tight-loop calls** through `begin_with_snapshots +
//! end_with_snapshots` against a fresh `Trace` per criterion
//! iteration. Simulates the `flat_calls.php` workload from
//! `SPECIFICATION.md` §9.2: same function called repeatedly, one
//! dict entry, no exception unwind. Reports the total wall-time
//! for the 10⁴-call inner loop and the implied per-call mean.
//!
//! Complements `recorder_hot_path.rs`:
//! - `recorder_hot_path` measures one call per iteration, with the
//!   trace persisting across criterion iterations (dictionary
//!   long-warm).
//! - `recorder_workload` measures a 10⁴-call workload against a
//!   freshly-constructed trace (one cold dict-miss + 9_999 hits
//!   per iteration — closer to a real request's hit ratio).
//!
//! The fresh-trace-per-iteration shape uses
//! `Criterion::bench_with_input` + `iter_batched` so criterion
//! constructs the input outside the timed region. The per-iteration
//! input setup is a `Trace::new` + a `Categorised` construction —
//! both sub-microsecond; criterion's `BatchSize::SmallInput`
//! amortises them across batches.
//!
//! Run with:
//!
//! ```sh
//! cargo bench -p php-analyze --features bench-seam --bench recorder_workload
//! ```

#[cfg(not(feature = "bench-seam"))]
compile_error!(
    "crates/php-analyze/benches/recorder_workload.rs requires the bench-seam feature: \
     run `cargo bench -p php-analyze --features bench-seam --bench recorder_workload` \
     (the feature gates the `php_analyze::bench_seam` module that this bench imports)"
);

#[cfg(feature = "bench-seam")]
use std::borrow::Cow;
#[cfg(feature = "bench-seam")]
use std::sync::Arc;

#[cfg(feature = "bench-seam")]
use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
#[cfg(feature = "bench-seam")]
use php_analyze::bench_seam::{
    begin_with_snapshots, end_with_snapshots, Categorised, EntrySnapshots, ExitSnapshots,
    FunctionKey, FunctionKind, RequestIdentity, Trace, TraceLimits,
};

/// Workload size matches the `flat_calls.php` workload from
/// `SPECIFICATION.md` §9.2 (10⁴ calls to one function).
#[cfg(feature = "bench-seam")]
const WORKLOAD_SIZE: usize = 10_000;

/// Construct a fresh `Trace` per iteration so the per-iteration
/// dictionary state is identical: one cold miss on the first call,
/// 9_999 hits on the rest. The trace's limits are large enough to
/// prevent any flush or cap-drop during the 10⁴-call inner loop
/// (a `CallRecord` is 64 bytes per `recorder::observer`'s
/// `CALL_RECORD_FIXED_BYTES`, so 10⁴ records ≈ 640 KB — well
/// under `usize::MAX`).
#[cfg(feature = "bench-seam")]
fn make_trace() -> Trace {
    let identity = RequestIdentity {
        host: Arc::from("bench-host"),
        sapi: Arc::from("cli"),
        pid: 0,
        uri_or_script: Arc::from("/bench/recorder_workload.rs"),
    };
    let limits = TraceLimits {
        flush_records: usize::MAX,
        flush_bytes: usize::MAX,
        buffer_cap_bytes: usize::MAX,
        max_depth: 1024,
    };
    Trace::new(identity, limits)
}

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
fn bench_workload(c: &mut Criterion) {
    c.bench_with_input(
        BenchmarkId::new("recorder_workload", WORKLOAD_SIZE),
        &WORKLOAD_SIZE,
        |b, &size| {
            // `iter_batched` constructs the `(trace, categorised)`
            // input outside the timed region. We can't use the
            // simpler `iter` because the 10⁴-call inner loop
            // mutates the trace's buffer — successive iterations
            // would grow the buffer monotonically and eventually
            // saturate cache. A fresh trace per iteration gives
            // every measurement the same starting conditions.
            //
            // `BatchSize::SmallInput` tells criterion that the
            // per-iteration setup is cheap enough to construct
            // many batches; the alternative (`PerIteration`) would
            // serialise input construction with measurement.
            b.iter_batched(
                || (make_trace(), make_categorised()),
                |(mut trace, categorised)| {
                    for _ in 0..size {
                        begin_with_snapshots(&mut trace, &categorised, ENTRY_SNAPSHOTS);
                        end_with_snapshots(&mut trace, EXIT_SNAPSHOTS, false);
                    }
                },
                BatchSize::SmallInput,
            );
        },
    );
}

#[cfg(feature = "bench-seam")]
criterion_group!(benches, bench_workload);
#[cfg(feature = "bench-seam")]
criterion_main!(benches);

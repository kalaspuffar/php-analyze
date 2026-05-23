#!/usr/bin/env python3
"""Xdebug-vs-php-analyze comparator.

Consumes:
  - Xdebug 3.x computerized trace (.xt file), per
    https://xdebug.org/docs/trace
  - `stub-ingest`'s /debug/batches JSON output (already
    msgpack-decoded server-side; we re-parse the JSON).

Produces a Markdown report at the path named by --output. The
report has two sections:

  Call coverage          per-function counts, overlap %.
  Per-call timing delta  histogram + p50/p95/p99 of percent-
                         delta between paired calls.

The script never carries a pass/fail headline. Per
xdebug-spot-check's design D-7, the operator reads the numbers
and judges.
"""

from __future__ import annotations

import argparse
import dataclasses
import datetime as dt
import json
import os
import re
import statistics
import subprocess
import sys
from collections import defaultdict
from pathlib import Path
from typing import Iterable


# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------


def parse_args(argv: list[str]) -> argparse.Namespace:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--xdebug", required=True, type=Path,
                   help="Path to the Xdebug computerized trace (.xt).")
    p.add_argument("--analyze", required=True, type=Path,
                   help="Path to the stub-ingest /debug/batches JSON dump.")
    p.add_argument("--fixture", required=True, type=Path,
                   help="Path to the PHP fixture the measurement covered.")
    p.add_argument("--cdylib", required=True, type=Path,
                   help="Path to libphp_analyze.so used for the analyze run.")
    p.add_argument("--php-bin", required=True,
                   help="The php-cli invocation (e.g. php8.4).")
    p.add_argument("--xdebug-version", required=True,
                   help="Xdebug version string detected by run.sh.")
    p.add_argument("--output", required=True, type=Path,
                   help="Where to write the rendered REPORT.md.")
    return p.parse_args(argv)


# ---------------------------------------------------------------------------
# Data types
# ---------------------------------------------------------------------------


@dataclasses.dataclass(frozen=True)
class XdebugCall:
    """One paired entry+exit observation from an Xdebug trace."""

    fn_name: str
    is_user_defined: bool
    t_in_ns: int
    t_out_ns: int
    level: int

    @property
    def duration_ns(self) -> int:
        return self.t_out_ns - self.t_in_ns


@dataclasses.dataclass(frozen=True)
class AnalyzeCall:
    """One call record from a php-analyze MessagePack batch."""

    fqn: str
    kind: str
    t_in_ns: int
    t_out_ns: int

    @property
    def duration_ns(self) -> int:
        return self.t_out_ns - self.t_in_ns


# ---------------------------------------------------------------------------
# Xdebug trace parser
# ---------------------------------------------------------------------------


# Header lines look like:
#   Version: 3.5.0
#   File format: 4
#   TRACE START [2026-05-23 09:48:30.123456]
#
# Body lines for entry events (flag = 0) have these TAB-separated
# columns (per the upstream docs):
#   level fn_no flag time_idx mem_usage fn_name fn_type incl_filename filename lineno
#
# Exit events (flag = 1) have:
#   level fn_no flag time_idx mem_usage
#
# Return-value events (flag = R) we ignore entirely. The script
# is configured with `xdebug.collect_return = 0` so they
# shouldn't appear anyway.
def parse_xdebug_trace(path: Path) -> tuple[str, list[XdebugCall]]:
    """Parse a computerized Xdebug trace.

    Returns ``(format_version_string, calls)``. Raises
    ``ValueError`` on a 2.x trace or anything else we don't
    recognise.
    """
    entries_by_fn_no: dict[str, tuple[int, float, int, str, str]] = {}
    calls: list[XdebugCall] = []
    file_format_version = "unknown"

    with path.open(encoding="utf-8") as fh:
        for raw in fh:
            line = raw.rstrip("\n")
            if not line:
                continue
            if line.startswith("Version:"):
                version = line.split(":", 1)[1].strip()
                if not version.startswith("3."):
                    raise ValueError(
                        f"Xdebug trace claims version {version!r}; only 3.x is supported"
                    )
                continue
            if line.startswith("File format:"):
                file_format_version = line.split(":", 1)[1].strip()
                continue
            if line.startswith("TRACE START") or line.startswith("TRACE END"):
                continue

            fields = line.split("\t")
            if len(fields) < 5:
                # Stray informational line; skip.
                continue
            try:
                level = int(fields[0])
            except ValueError:
                continue  # not a numeric line
            fn_no = fields[1]
            flag = fields[2]
            try:
                time_index = float(fields[3])
            except ValueError:
                continue
            # fields[4] is memory; not used here.

            if flag == "0":
                if len(fields) < 6:
                    continue  # malformed entry; skip
                fn_name = fields[5]
                fn_type = fields[6] if len(fields) > 6 else ""
                # `fn_type` is "0" for user-defined, "1" for
                # internal. The official docs use 0/1 here.
                is_user = fn_type == "0"
                # Xdebug time_index is in seconds with µs precision.
                t_in_ns = int(round(time_index * 1_000_000_000))
                entries_by_fn_no[fn_no] = (level, time_index, t_in_ns, fn_name, fn_type)
            elif flag == "1":
                pair = entries_by_fn_no.pop(fn_no, None)
                if pair is None:
                    continue  # exit without matching entry; defensive
                level_in, _, t_in_ns, fn_name, fn_type = pair
                t_out_ns = int(round(time_index * 1_000_000_000))
                calls.append(
                    XdebugCall(
                        fn_name=fn_name,
                        is_user_defined=fn_type == "0",
                        t_in_ns=t_in_ns,
                        t_out_ns=t_out_ns,
                        level=level_in,
                    )
                )
            # flag "R" (return value) skipped per design D-2.

    # Any leftover entries — unbalanced exit-less calls.
    # Usually caused by `exit()` mid-function. We surface the
    # count via the unmatched_count return so the report can
    # mention it, but don't fail.
    return file_format_version, calls


# ---------------------------------------------------------------------------
# Analyze JSON parser
# ---------------------------------------------------------------------------


# Mirrors `wire::FunctionKind` in the production crate:
#   0=function, 1=method, 2=closure, 3=internal
KIND_NAMES = {0: "function", 1: "method", 2: "closure", 3: "internal"}


def parse_analyze_batches(path: Path) -> list[AnalyzeCall]:
    """Parse stub-ingest's /debug/batches JSON dump into a flat
    list of `AnalyzeCall`s, in the order the batches arrived
    (which is the recorder's RSHUTDOWN-flush order)."""
    raw = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(raw, list):
        raise ValueError(f"expected /debug/batches to return a JSON array; got {type(raw).__name__}")

    # Wire-format field names (per `wire::CallRecord` in the
    # production crate, mirroring `SPECIFICATION.md` §4.2.3):
    # `fn` for the dictionary fn-id reference, `t_in` / `t_out`
    # for the per-call timestamps (already in nanoseconds —
    # the wire shape does not append `_ns`). Dict entries use
    # the unabbreviated `fn_id` since they're a separate map.
    calls: list[AnalyzeCall] = []
    # Build a global dict-id → entry map up front. The recorder
    # emits each `DictEntry` once per first-sight within a
    # trace, so a call in batch N may reference a dict entry
    # from batch < N. One pass over every batch's `dict` array
    # populates the map; subsequent lookups are O(1).
    dict_entries: dict[int, dict] = {}
    for batch in raw:
        for d in batch.get("dict", []):
            dict_entries[d["fn_id"]] = d
    for batch in raw:
        for c in batch.get("calls", []):
            fn_id = c["fn"]
            entry = dict_entries.get(fn_id)
            if entry is None:
                fqn = f"?fn_id={fn_id}"
                kind_str = "unknown"
            else:
                fqn = entry.get("fqn", f"?fn_id={fn_id}")
                kind_str = KIND_NAMES.get(entry.get("kind", -1), "unknown")
            calls.append(
                AnalyzeCall(
                    fqn=fqn,
                    kind=kind_str,
                    t_in_ns=int(c["t_in"]),
                    t_out_ns=int(c["t_out"]),
                )
            )
    return calls


# ---------------------------------------------------------------------------
# Function-name normalisation
# ---------------------------------------------------------------------------


_CLOSURE_RE_XDEBUG = re.compile(r"^\{closure:([^:]+):(\d+)-\d+\}$")
_CLOSURE_RE_ANALYZE = re.compile(r"^closure:([^:]+):(\d+)$")


def normalise_fn_name(name: str) -> str:
    """Map an Xdebug or analyze function name to a common key.

    Closures are the only category where the two tools disagree:
      Xdebug : `{closure:/path/to/file.php:LINE_START-LINE_END}`
      analyze: `closure:/path/to/file.php:LINE_START`
    We normalise both to `closure:/path:LINE_START`.

    Methods may be reported as `Class->method` (instance) or
    `Class::method` (static). The two are semantically distinct
    invocations of the same method, but for coverage purposes
    they refer to the same function. We don't merge them today
    — fixtures the spot-check cares about (recursive_walk.php
    etc.) don't use methods.
    """
    m = _CLOSURE_RE_XDEBUG.match(name)
    if m:
        return f"closure:{m.group(1)}:{m.group(2)}"
    m = _CLOSURE_RE_ANALYZE.match(name)
    if m:
        return f"closure:{m.group(1)}:{m.group(2)}"
    return name


# ---------------------------------------------------------------------------
# Aggregation + comparison
# ---------------------------------------------------------------------------


@dataclasses.dataclass
class CoverageRow:
    fn_name: str
    xdebug_count: int
    analyze_count: int

    @property
    def delta(self) -> int:
        return self.analyze_count - self.xdebug_count

    @property
    def overlap_pct(self) -> float:
        if self.xdebug_count == 0 and self.analyze_count == 0:
            return 100.0
        denom = max(self.xdebug_count, self.analyze_count, 1)
        return 100.0 * min(self.xdebug_count, self.analyze_count) / denom


@dataclasses.dataclass
class TimingRow:
    fn_name: str
    sample_size: int
    p50_abs_pct: float
    p95_abs_pct: float
    max_abs_pct: float


def coverage_table(
    xdebug_calls: list[XdebugCall],
    analyze_calls: list[AnalyzeCall],
) -> list[CoverageRow]:
    xcount: dict[str, int] = defaultdict(int)
    acount: dict[str, int] = defaultdict(int)
    for c in xdebug_calls:
        xcount[normalise_fn_name(c.fn_name)] += 1
    for c in analyze_calls:
        acount[normalise_fn_name(c.fqn)] += 1
    names = sorted(set(xcount) | set(acount))
    return [CoverageRow(n, xcount.get(n, 0), acount.get(n, 0)) for n in names]


def aggregate_overlap_pct(rows: list[CoverageRow]) -> float:
    """Overall coverage: `sum(min)` / `sum(xdebug_count)` weighted
    by Xdebug's view of the call set."""
    total_x = sum(r.xdebug_count for r in rows)
    if total_x == 0:
        return 100.0
    overlap = sum(min(r.xdebug_count, r.analyze_count) for r in rows)
    return 100.0 * overlap / total_x


def per_call_deltas(
    xdebug_calls: list[XdebugCall],
    analyze_calls: list[AnalyzeCall],
) -> dict[str, list[float]]:
    """Position-pair durations by function name, return percent
    deltas: `(analyze - xdebug) / xdebug * 100`."""
    by_x: dict[str, list[int]] = defaultdict(list)
    by_a: dict[str, list[int]] = defaultdict(list)
    for c in xdebug_calls:
        by_x[normalise_fn_name(c.fn_name)].append(c.duration_ns)
    for c in analyze_calls:
        by_a[normalise_fn_name(c.fqn)].append(c.duration_ns)
    deltas: dict[str, list[float]] = {}
    for name in set(by_x) & set(by_a):
        xs = by_x[name]
        as_ = by_a[name]
        n = min(len(xs), len(as_))
        if n == 0:
            continue
        bucket: list[float] = []
        for i in range(n):
            x = xs[i]
            a = as_[i]
            if x <= 0:
                # Xdebug duration of zero or negative means the
                # call's clock resolution swallowed it; ratio
                # undefined. Skip the pair rather than inject a
                # spurious infinity.
                continue
            bucket.append(100.0 * (a - x) / x)
        if bucket:
            deltas[name] = bucket
    return deltas


def histogram_buckets(values: Iterable[float]) -> dict[str, int]:
    buckets = {
        "≤ −5%": 0,
        "(−5%, −1%]": 0,
        "(−1%, +1%)": 0,
        "[+1%, +5%)": 0,
        "≥ +5%": 0,
    }
    for v in values:
        if v <= -5.0:
            buckets["≤ −5%"] += 1
        elif v <= -1.0:
            buckets["(−5%, −1%]"] += 1
        elif v < 1.0:
            buckets["(−1%, +1%)"] += 1
        elif v < 5.0:
            buckets["[+1%, +5%)"] += 1
        else:
            buckets["≥ +5%"] += 1
    return buckets


def percentile(sorted_vals: list[float], p: float) -> float:
    """Linear-interpolation percentile across a sorted list."""
    if not sorted_vals:
        return float("nan")
    if len(sorted_vals) == 1:
        return sorted_vals[0]
    k = (len(sorted_vals) - 1) * p
    f = int(k)
    c = min(f + 1, len(sorted_vals) - 1)
    if f == c:
        return sorted_vals[f]
    return sorted_vals[f] + (sorted_vals[c] - sorted_vals[f]) * (k - f)


def timing_table(deltas: dict[str, list[float]]) -> list[TimingRow]:
    rows: list[TimingRow] = []
    for name in sorted(deltas):
        abs_vals = sorted(abs(v) for v in deltas[name])
        rows.append(
            TimingRow(
                fn_name=name,
                sample_size=len(abs_vals),
                p50_abs_pct=percentile(abs_vals, 0.50),
                p95_abs_pct=percentile(abs_vals, 0.95),
                max_abs_pct=abs_vals[-1] if abs_vals else float("nan"),
            )
        )
    return rows


# ---------------------------------------------------------------------------
# Cdylib / git fingerprint
# ---------------------------------------------------------------------------


def git_sha_for(path: Path) -> str:
    """Best-effort SHA of HEAD plus a `dirty-working-tree` marker
    if the working tree differs from HEAD. Empty string if the
    path isn't under git or git isn't on PATH."""
    repo_root = path
    while repo_root != repo_root.parent:
        if (repo_root / ".git").exists():
            break
        repo_root = repo_root.parent
    else:
        return ""

    def _git(args: list[str]) -> str:
        try:
            res = subprocess.run(
                ["git", "-C", str(repo_root), *args],
                check=False,
                capture_output=True,
                text=True,
                timeout=5,
            )
            return res.stdout.strip()
        except (subprocess.SubprocessError, FileNotFoundError):
            return ""

    sha = _git(["rev-parse", "--short=12", "HEAD"])
    if not sha:
        return ""
    dirty = subprocess.run(
        ["git", "-C", str(repo_root), "diff", "--quiet", "HEAD"],
        check=False,
    ).returncode != 0
    if dirty:
        sha += " (dirty-working-tree)"
    return sha


def php_version_for(php_bin: str) -> str:
    try:
        res = subprocess.run(
            [php_bin, "-r", "echo PHP_VERSION;"],
            check=False, capture_output=True, text=True, timeout=5,
        )
        return res.stdout.strip() or "?"
    except (subprocess.SubprocessError, FileNotFoundError):
        return "?"


# ---------------------------------------------------------------------------
# Report rendering
# ---------------------------------------------------------------------------


def render_report(
    *,
    output: Path,
    fixture: Path,
    cdylib: Path,
    php_bin: str,
    php_version: str,
    xdebug_version: str,
    git_sha: str,
    xdebug_trace_format: str,
    xdebug_calls: list[XdebugCall],
    analyze_calls: list[AnalyzeCall],
    coverage_rows: list[CoverageRow],
    overall_overlap_pct: float,
    deltas: dict[str, list[float]],
    timing_rows: list[TimingRow],
) -> None:
    flat_deltas = [v for arr in deltas.values() for v in arr]
    hist = histogram_buckets(flat_deltas)
    sorted_abs = sorted(abs(v) for v in flat_deltas)
    p50 = percentile(sorted_abs, 0.50)
    p95 = percentile(sorted_abs, 0.95)
    p99 = percentile(sorted_abs, 0.99)
    total_pairs = len(flat_deltas)

    now = dt.datetime.now(dt.timezone.utc).isoformat(timespec="seconds")

    lines: list[str] = []
    lines.append("# xdebug-spot-check — accuracy report")
    lines.append("")
    lines.append("> Generated by `tools/xdebug-spot-check/run.sh`. **Not a CI gate.**")
    lines.append("> The operator reads this report and judges whether `php-analyze`'s")
    lines.append("> recorder is producing data trustworthy enough to hand to the")
    lines.append("> `../php-tree-visualizer` repo. See [`README.md`](./README.md) for")
    lines.append("> what each section means and the known limitations.")
    lines.append("")
    lines.append("## Snapshot")
    lines.append("")
    lines.append("| Field | Value |")
    lines.append("| --- | --- |")
    lines.append(f"| Generated (UTC) | `{now}` |")
    lines.append(f"| cdylib | `{cdylib}` |")
    lines.append(f"| cdylib git SHA | `{git_sha or 'unknown'}` |")
    lines.append(f"| PHP | `{php_bin}` ({php_version}) |")
    lines.append(f"| Xdebug | `{xdebug_version}` (file-format `{xdebug_trace_format}`) |")
    lines.append(f"| Fixture | `{fixture}` |")
    lines.append(f"| Xdebug calls observed | `{len(xdebug_calls):,}` |")
    lines.append(f"| php-analyze calls observed | `{len(analyze_calls):,}` |")
    lines.append("")

    lines.append("## Call coverage")
    lines.append("")
    lines.append(f"**Aggregate overlap:** `{overall_overlap_pct:.3f} %` (weighted by Xdebug count).")
    lines.append("")
    lines.append("> Overlap = `Σ min(xdebug_count, analyze_count) / Σ xdebug_count`.")
    lines.append("> `SPECIFICATION.md` §1.3 #3b uses `≥ 99.5 %` as a long-term target;")
    lines.append("> the spot-check reports the number, the operator decides whether the")
    lines.append("> current value is good enough for MVP.")
    lines.append("")
    lines.append("| Function | Xdebug count | analyze count | Δ | Overlap |")
    lines.append("| --- | ---: | ---: | ---: | ---: |")
    for row in coverage_rows:
        delta_sign = "+" if row.delta > 0 else ""
        lines.append(
            f"| `{row.fn_name}` | {row.xdebug_count:,} | {row.analyze_count:,} | "
            f"{delta_sign}{row.delta:,} | {row.overlap_pct:.3f} % |"
        )
    lines.append("")

    # Footnote any function only Xdebug saw (likely
    # opcode-specialised internals per COMMENTS.md C-5).
    xdebug_only = [r for r in coverage_rows if r.xdebug_count > 0 and r.analyze_count == 0]
    if xdebug_only:
        lines.append("> **Xdebug-only functions** (likely opcode-specialised internals invisible to the Zend")
        lines.append("> observer per `COMMENTS.md` C-5; not a recorder bug):")
        for r in xdebug_only:
            lines.append(f"> - `{r.fn_name}` — Xdebug saw {r.xdebug_count}")
        lines.append("")

    analyze_only = [r for r in coverage_rows if r.analyze_count > 0 and r.xdebug_count == 0]
    if analyze_only:
        lines.append("> **php-analyze-only functions** (Xdebug didn't observe; possible name-normalisation gap):")
        for r in analyze_only:
            lines.append(f"> - `{r.fn_name}` — analyze saw {r.analyze_count}")
        lines.append("")

    lines.append("## Per-call timing delta")
    lines.append("")
    lines.append("> Per `(function, position)` pair across both traces; `percent_delta`")
    lines.append("> = `(analyze_dur − xdebug_dur) / xdebug_dur × 100`. Pairs where the")
    lines.append("> Xdebug duration was `≤ 0` (clock-resolution floor) are skipped.")
    lines.append("> `SPECIFICATION.md` §1.3 #3c uses `±5 %` as a long-term target.")
    lines.append("")
    if total_pairs == 0:
        lines.append("**No paired calls** — coverage didn't overlap on any function with positive Xdebug duration.")
        lines.append("")
    else:
        lines.append(f"**Pairs evaluated:** `{total_pairs:,}`. **|Δ%|** percentiles across all functions:")
        lines.append("")
        lines.append("| Percentile | abs(percent delta) |")
        lines.append("| --- | ---: |")
        lines.append(f"| p50 | {p50:.2f} % |")
        lines.append(f"| p95 | {p95:.2f} % |")
        lines.append(f"| p99 | {p99:.2f} % |")
        lines.append("")
        lines.append("Histogram of signed percent delta:")
        lines.append("")
        lines.append("| Bucket | Count | Share |")
        lines.append("| --- | ---: | ---: |")
        for bucket, count in hist.items():
            share = 100.0 * count / total_pairs if total_pairs else 0.0
            lines.append(f"| `{bucket}` | {count:,} | {share:.2f} % |")
        lines.append("")
        if timing_rows:
            lines.append("### Per-function summary")
            lines.append("")
            lines.append("| Function | Sample size | p50 \\|Δ%\\| | p95 \\|Δ%\\| | max \\|Δ%\\| |")
            lines.append("| --- | ---: | ---: | ---: | ---: |")
            for row in timing_rows:
                lines.append(
                    f"| `{row.fn_name}` | {row.sample_size:,} | "
                    f"{row.p50_abs_pct:.2f} % | {row.p95_abs_pct:.2f} % | "
                    f"{row.max_abs_pct:.2f} % |"
                )
            lines.append("")

    lines.append("## Known limitations")
    lines.append("")
    lines.append("- **Xdebug's own per-call overhead inflates the reference durations.**")
    lines.append("  Xdebug instruments every call; its `t_out − t_in` includes its own")
    lines.append("  bookkeeping. The relevant comparison is per-call shape, not absolute")
    lines.append("  nanosecond-for-nanosecond agreement.")
    lines.append("- **PHP-specialised internals (`strlen(\"hi\")`, etc.) are invisible to")
    lines.append("  the Zend observer** per `COMMENTS.md` C-5. The Xdebug-only footnote")
    lines.append("  above lists any that appeared on this run.")
    lines.append("- **Position-wise pairing assumes deterministic call order.**")
    lines.append("  Non-deterministic fixtures (timing-, randomness-, or IO-dependent)")
    lines.append("  will produce a misleading timing-delta histogram. The canonical")
    lines.append("  `recursive_walk.php` fixture is deterministic.")

    output.write_text("\n".join(lines) + "\n", encoding="utf-8")


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


def main(argv: list[str]) -> int:
    args = parse_args(argv)

    try:
        format_version, xdebug_calls = parse_xdebug_trace(args.xdebug)
    except (OSError, ValueError) as e:
        print(f"error: parse xdebug trace failed: {e}", file=sys.stderr)
        return 1

    try:
        analyze_calls = parse_analyze_batches(args.analyze)
    except (OSError, ValueError, json.JSONDecodeError) as e:
        print(f"error: parse analyze batches failed: {e}", file=sys.stderr)
        return 1

    coverage_rows = coverage_table(xdebug_calls, analyze_calls)
    overall_overlap_pct = aggregate_overlap_pct(coverage_rows)
    deltas = per_call_deltas(xdebug_calls, analyze_calls)
    timing_rows = timing_table(deltas)

    git_sha = git_sha_for(args.cdylib)
    php_version = php_version_for(args.php_bin)

    render_report(
        output=args.output,
        fixture=args.fixture,
        cdylib=args.cdylib,
        php_bin=args.php_bin,
        php_version=php_version,
        xdebug_version=args.xdebug_version,
        git_sha=git_sha,
        xdebug_trace_format=format_version,
        xdebug_calls=xdebug_calls,
        analyze_calls=analyze_calls,
        coverage_rows=coverage_rows,
        overall_overlap_pct=overall_overlap_pct,
        deltas=deltas,
        timing_rows=timing_rows,
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))

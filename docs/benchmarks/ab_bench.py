"""Interleaved A/B pepper-sync runs.

Session-level machine drift lands on every run of a sequential benchmark
session, so a before tree measured at 2pm and an after tree measured at 3pm
carry an offset no amount of extra runs removes. This driver builds each
side in its own worktree, sharing the target directory, then runs the
scenarios alternately, before, after, before, after, so drift applies to
both sides alike. It writes docs/benchmarks/pepper-sync-<label>.jsonl for
each label, ready for compare.py.

Both sides build the harness at the after commit, so the measured
difference is the dependency change alone: the before worktree checks out
the same commit and swaps in only the Cargo.toml and Cargo.lock of the
before commit. The build happens up front, on both sides, before any
measuring, so the first scenario is not measured against a cold machine.
"""

import argparse
import os
import re
import subprocess
import sys
from pathlib import Path


def run(cmd, **kwargs):
    print("+", " ".join(cmd), flush=True)
    subprocess.run(cmd, check=True, **kwargs)


def short(commit):
    return subprocess.check_output(["git", "rev-parse", "--short", commit],
                                   cwd=root, text=True).strip()


def build(label, commit, deps_from, tmp, env, deps):
    tree = tmp / f"tree-{label}"
    run(["git", "worktree", "add", "--detach", str(tree), commit], cwd=root)
    if deps_from is not None:
        run(["git", "checkout", deps_from, "--", "Cargo.toml", "Cargo.lock"], cwd=tree)
    run(["cargo", "bench", "-p", "zingolib", "--bench", "pepper_sync", "--no-run"],
        cwd=tree, env=env)
    exe = max(
        (p for p in deps.glob("pepper_sync-*") if p.is_file() and p.suffix != ".d"),
        key=lambda p: p.stat().st_mtime,
    )
    print(f"  {label}: {exe}", flush=True)
    return tree, exe


ap = argparse.ArgumentParser(description=__doc__)
ap.add_argument("before", help="commit whose Cargo.toml and Cargo.lock define the before deps")
ap.add_argument("after", help="commit whose harness and deps define the after side")
ap.add_argument("--runs", type=int, default=3)
ap.add_argument("--blocks", type=int, default=3000)
ap.add_argument("--keep", action="store_true", help="keep the worktrees for inspection")
args = ap.parse_args()

root = Path(subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip())
bench_rs = root / "zingolib" / "benches" / "pepper_sync.rs"
scenarios = re.findall(r'Scenario \{\s*name: "([^"]+)"', bench_rs.read_text())
if not scenarios:
    sys.exit("no scenarios found in the bench source")

after = short(args.after)
before_label = short(args.before)

tmp = root / "target" / "ab"
tmp.mkdir(parents=True, exist_ok=True)
env = {**os.environ, "CARGO_TARGET_DIR": str(root / "target")}
deps = root / "target" / "release" / "deps"

try:
    before_tree, before_exe = build(before_label, after, args.before, tmp, env, deps)
    after_tree, after_exe = build(after, after, None, tmp, env, deps)

    pieces = {before_label: [], after: []}
    for scenario in scenarios:
        for label, exe in ((before_label, before_exe), (after, after_exe)):
            piece = tmp / f"{label}-{scenario}.jsonl"
            run([str(exe), scenario], cwd=root, env={
                **env,
                "PEPPER_BENCH_OUT": str(piece),
                "PEPPER_BENCH_RUNS": str(args.runs),
                "PEPPER_BENCH_BLOCKS": str(args.blocks),
            })
            pieces[label].append(piece)

    for label in (before_label, after):
        merged = root / "docs" / "benchmarks" / f"pepper-sync-{label}.jsonl"
        with merged.open("wb") as out:
            for piece in pieces[label]:
                out.write(piece.read_bytes())
        print(f"wrote {merged}", flush=True)
finally:
    if not args.keep:
        for tree in (tmp / f"tree-{before_label}", tmp / f"tree-{after}"):
            subprocess.run(["git", "worktree", "remove", "--force", str(tree)],
                           cwd=root, capture_output=True)

print(f"now: just compare {before_label} {after}")


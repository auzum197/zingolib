"""Interleaved A/B pepper-sync runs.

Session-level machine drift lands on every run of a sequential benchmark
session, so a before tree measured at 2pm and an after tree measured at 3pm
carry an offset no amount of extra runs removes. This driver builds each
commit in its own worktree, sharing the target directory, then runs the
scenarios alternately, before, after, before, after, so drift applies to
both sides alike. It writes docs/benchmarks/pepper-sync-<label>.jsonl for
each label, ready for compare.py.

The build happens up front, on both trees, before any measuring, so the
first scenario is not measured against a cold machine.
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


def build(commit, tmp, env, deps):
    tree = tmp / f"tree-{commit}"
    run(["git", "worktree", "add", "--detach", str(tree), commit], cwd=root)
    run(["cargo", "bench", "-p", "zingolib", "--bench", "pepper_sync", "--no-run"],
        cwd=tree, env=env)
    exe = max(
        (p for p in deps.glob("pepper_sync-*") if p.is_file() and p.suffix != ".d"),
        key=lambda p: p.stat().st_mtime,
    )
    print(f"  {commit}: {exe}", flush=True)
    return tree, exe


ap = argparse.ArgumentParser(description=__doc__)
ap.add_argument("before", help="commit-ish of the before tree, used as the jsonl label")
ap.add_argument("after", help="commit-ish of the after tree, used as the jsonl label")
ap.add_argument("--runs", type=int, default=5)
ap.add_argument("--blocks", type=int, default=3000)
ap.add_argument("--keep", action="store_true", help="keep the worktrees for inspection")
args = ap.parse_args()

root = Path(subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip())
bench_rs = root / "zingolib" / "benches" / "pepper_sync.rs"
scenarios = re.findall(r'Scenario \{\s*name: "([^"]+)"', bench_rs.read_text())
if not scenarios:
    sys.exit("no scenarios found in the bench source")

tmp = root / "target" / "ab"
tmp.mkdir(parents=True, exist_ok=True)
env = {**os.environ, "CARGO_TARGET_DIR": str(root / "target")}
deps = root / "target" / "release" / "deps"

try:
    before_tree, before_exe = build(args.before, tmp, env, deps)
    after_tree, after_exe = build(args.after, tmp, env, deps)

    pieces = {args.before: [], args.after: []}
    for scenario in scenarios:
        for label, exe in ((args.before, before_exe), (args.after, after_exe)):
            piece = tmp / f"{label}-{scenario}.jsonl"
            run([str(exe), scenario], cwd=root, env={
                **env,
                "PEPPER_BENCH_OUT": str(piece),
                "PEPPER_BENCH_RUNS": str(args.runs),
                "PEPPER_BENCH_BLOCKS": str(args.blocks),
            })
            pieces[label].append(piece)

    for label in (args.before, args.after):
        merged = root / "docs" / "benchmarks" / f"pepper-sync-{label}.jsonl"
        with merged.open("wb") as out:
            for piece in pieces[label]:
                out.write(piece.read_bytes())
        print(f"wrote {merged}", flush=True)
finally:
    if not args.keep:
        for tree in (tmp / f"tree-{args.before}", tmp / f"tree-{args.after}"):
            subprocess.run(["git", "worktree", "remove", "--force", str(tree)],
                           cwd=root, capture_output=True)

print(f"now: just compare {args.before} {args.after}")

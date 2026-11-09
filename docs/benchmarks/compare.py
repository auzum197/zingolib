import json
import statistics
import sys
from collections import defaultdict


def load(path):
    runs = defaultdict(list)
    with open(path) as f:
        for line in f:
            r = json.loads(line)
            runs[(r["scenario"], r["variant"], r["phase"])].append(r["seconds"])
    return runs


def best_median(runs):
    return {k: (min(v), statistics.median(v)) for k, v in runs.items()}


band = float(sys.argv[3]) if len(sys.argv) > 3 else 0.03

base = best_median(load(sys.argv[1]))
new = best_median(load(sys.argv[2]))


def mark(ratio):
    return f"{ratio:.2f}{'≈' if abs(ratio - 1) <= band else ''}"


print(f"{'scenario':24} {'variant':16} {'phase':11} {'before':>7} {'after':>7} {'best':>8} {'median':>8}")
for key in sorted(base):
    if key not in new:
        continue
    before_best, before_med = base[key]
    after_best, after_med = new[key]
    print(
        f"{key[0]:24} {key[1]:16} {key[2]:11} {before_best:7.2f} {after_best:7.2f}"
        f" {mark(after_best / before_best):>8} {mark(after_med / before_med):>8}"
    )


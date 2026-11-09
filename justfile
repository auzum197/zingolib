# Pepper-sync benchmarks. Run `just bench` once before touching a dependency
# pin and once after, then `just compare <before> <after>`. Labels default to
# the current commit hash, matching the file names in docs/benchmarks/.

bench label=`git rev-parse --short HEAD`:
    PEPPER_BENCH_OUT="{{ justfile_directory() }}/docs/benchmarks/pepper-sync-{{ label }}.jsonl" cargo bench -p zingolib --bench pepper_sync

compare before after:
    python3 "{{ justfile_directory() }}/docs/benchmarks/compare.py" \
        "{{ justfile_directory() }}/docs/benchmarks/pepper-sync-{{ before }}.jsonl" \
        "{{ justfile_directory() }}/docs/benchmarks/pepper-sync-{{ after }}.jsonl"

# Interleaved A/B: builds the harness once per side, swapping only
# Cargo.toml and Cargo.lock for the before side, then alternates scenarios
# between the two so session drift hits both sides. PEPPER_BENCH_VARIANTS
# narrows variants, e.g. the default levels only. Writes both jsonl files.
ab before after runs="3" blocks="3000":
    python3 "{{ justfile_directory() }}/docs/benchmarks/ab_bench.py" {{ before }} {{ after }} \
        --runs {{ runs }} --blocks {{ blocks }}

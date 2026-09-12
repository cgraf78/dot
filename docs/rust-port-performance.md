# Rust port performance

The native `dot` substantially reduces both command startup time and complete
update latency. Measurements below were taken on `nas` on 2026-09-07. The
historical Bash side was checked out at pre-cutover revision `e636d42`; the Rust
side was built from the cutover worktree with `cargo build --release --locked`.
All fixtures used local Git repositories, so network latency is excluded.

## Results

| Workload | Bash | Rust | Improvement |
| --- | ---: | ---: | ---: |
| `dot help`, mean of 50 | 19.8 ms | 10.7 ms | 1.85x faster |
| `dot version`, mean of 50 | 29.3 ms | 10.6 ms | 2.76x faster |
| clean full update, p95 of 20 | 1,962 ms | 641 ms | 3.06x faster |
| dirty full update, p95 of 20 | 2,267 ms | 804 ms | 2.82x faster |

## Live workload

The cutover binary was also measured against the installed `nas` workload,
where an update checks four repositories, three active overlays, 89 current
tools (plus one intentionally skipped tool), and 26 configuration hooks. The
installed pre-cutover Bash client and the release Rust binary were run with
`update --quiet`; every recorded sample exited zero with empty stdout and
stderr.

| Implementation | Samples | Median |
| --- | --- | ---: |
| Bash | 21,690 ms; 22,086 ms; 22,150 ms | 22,086 ms |
| Rust | 11,046 ms; 11,362 ms; 11,637 ms | 11,362 ms |

The live median is **1.94x faster**, reducing end-to-end wall time by about
49%. Unlike the synthetic fixture, this includes the machine's real provider
inventory and configuration hooks. The alternating runs were performed only
after both implementations completed the same workload successfully; failed
parity-diagnostic runs were excluded.

The full-update figures are the primary result: the native engine reduces p95
latency by about 67% for a clean fixture and 65% when converging an upstream
change. Startup measurements used `hyperfine --warmup 10 --runs 50
--shell=none`. `scripts/benchmark-port.sh` reproduces those four startup
commands when given explicit historical and native executables.

The update measurements use the same fixture shape and percentile calculation
on both revisions. Every dirty sample receives a distinct upstream commit
before timing begins, so all 20 samples measure actual convergence rather than
a mixture of one dirty run and clean steady-state runs. Run the current
correctness-and-budget measurement with:

```console
cargo test --release --locked --test perf_update -- --ignored --nocapture
```

To reproduce the Bash baseline with the exact same compiled harness, set
`DOT_PERF_EXECUTABLE` to the historical `bin/dot` and
`DOT_PERF_SOURCE_ROOT` to its checkout. The comparison above used revision
`e636d42` for the Bash baseline and the exact staged cutover tree for the Rust
path. `scripts/benchmark-port.sh` runs both full-update measurements after its
startup comparison when invoked from the native source root.

These are local-engine measurements, not promises about remote fetch time.
Repository size, filesystem cache state, host load, and network latency can all
change absolute wall-clock results. The checked-in native regression budget is
therefore deliberately looser than this reference-host observation while still
detecting a material performance regression.

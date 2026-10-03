# WROSECODE benchmarks

Run `scripts/benchmark.sh` to execute the full Rust test suite and write the
measured test latency plus flag detection corpus size to
`.ctf/reports/benchmark.json`.

Provider throughput and latency depend on the selected remote model and
network path. The live dashboard records model latency percentiles and token
usage from real provider responses instead of publishing a fixed synthetic
number.

The flag corpus test covers direct, base64, hexadecimal, and ROT13 detection.
Its current result and elapsed test time are kept in the generated JSON report.

`/export` or `Ctrl+E` writes the live measurements to a directory as
`metrics.json`, `metrics.csv` (per-model rows), and `latency.csv`
(`bucket_upper_ms,count`, `inf` for the overflow bucket) — the same histogram the
dashboard draws as a sparkline.

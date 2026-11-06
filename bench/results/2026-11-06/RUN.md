# How these results were produced (6 Nov 2026)

All runs were on one machine, one after another, from the repository root. Each e2e JSON
records `provenance.rayap_revision`, the exact simulator command, the resolved simulator
spec and the crawler settings.

| File | Command |
|---|---|
| `e2e-10k-run{1,2,3}.json` | `rayap-bench e2e --hosts 10000 --delay-ms 2000 --concurrency 8192 --max-seconds 900` |
| `e2e-sweep-{2500,5000,20000,40000}.json` | same with `--hosts N` (one run each) |
| `scheduler-100k.json` | `rayap-bench scheduler --hosts 100000 --pages-per-host 50 --delay-ms 2000 --request-ms 200 --in-flight 100000` |
| `e2e-10k-flamegraph.json`, `flamegraph-after.svg` | the 10k command with `--flamegraph` (the profiler adds load; not used for throughput) |
| `after-f6a9321-c4096-run{1,2}.json` | `rayap-bench e2e --hosts 10000 --delay-ms 2000 --concurrency 4096 --max-seconds 900` |
| `before-05a557a-c4096-run{1,2}.json` | the same command, using `rayap-bench` built at commit `05a557a` (before the optimisation), with `--sim-bin` pointing at the simulator built at `f6a9321`, so both versions face the same corrected referee. Runs alternated: before 1, after 1, before 2, after 2 |
| `before-05a557a-flamegraph.json`, `flamegraph-before.svg` | the before binary with `--flamegraph`, same simulator |

The `05a557a` bench binary predates the provenance fields, so its JSON files have no
`provenance` block, and its throughput fields use an older steady-window definition.
Compare before and after only on `wall_seconds`, `seconds_to_90pct_of_pages`,
`pages_per_second.overall`, `crawler_cpu_seconds`, `crawler_peak_rss_mib` and `referee`.
These fields mean the same thing in both versions.

Flamegraph shares: the largest single frame's percentage of all samples for a function
name. That frame includes everything it calls, so shares overlap and do not add up.

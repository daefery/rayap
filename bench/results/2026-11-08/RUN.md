# How these results were produced (8 Nov 2026)

Sharded coordinators (`--shards N`), code at `HEAD` of this commit, same simulator and
referee for every run, one laptop (i7-12650H, WSL2), load average 0.8–2.3 except where
noted. All runs finished; referee 0 proven violations, 0 uncertain in each.

| File | Command |
|---|---|
| `shards-s{1,2,4,8}-10k.json` | `rayap-bench e2e --hosts 10000 --delay-ms 2000 --concurrency 8192 --shards N --max-seconds 900` |
| `shards-s{1,4}-20k.json` | same with `--hosts 20000` |
| `shards-s1-10k-disturbed.json` | the first 1-shard 10k run; load average rose to 10 as it started (another process), wall 744 s vs 411 s for the rerun. Kept, not used in the tables. `shards-s1-10k-b.json` is the rerun. |

`coordinator_busy_fraction_estimate` is per shard (summed busy time divided by shards).

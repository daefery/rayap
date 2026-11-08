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

## After main-content extraction and no-rewrite-on-unchanged (afternoon, `dd52f50`/`43d743a`)

| File | Command |
|---|---|
| `lighter-writes-s{1,4,8}-10k.json`, `lighter-writes-s4-20k.json` | the same commands as the morning table, binary at `dd52f50` |
| `lighter-writes-s1-10k-collapse.json` | 1 shard, 10k, binary at `43d743a` (single-pass whitespace collapsing) |

Not comparable with the morning: the simulator's own CPU for identical work rose from
30–34 s to 40–52 s (10k) and 68 to 70 s (20k) between the two sets, so the machine was
slower in the afternoon (load average 2–4 at times, thermal state unknown). The 1-shard
result fell from 1,727 to 1,190 pages/s with the simulator 30% slower; the 4-shard 20k
result rose from 2,259 to 2,609. No conclusion about the write changes is drawn from this
set; it is kept so the next quiet-machine pair can be checked against it.

# How these results were produced (7 Nov 2026)

Revisit-policy (freshness) benchmark. Pure simulation, no network: `rayap-bench freshness`
draws hidden Poisson change rates, generates every change time once, and runs the three
policies against the same changes with the same fetch budget. Code at `43c0c16`
(`provenance.rayap_revision` in each file). One laptop (i7-12650H, WSL2), about 8 s per file.

| File | Command |
|---|---|
| `freshness-static{0.3,0.7,0.9,0.97}-seed{1,2,3}.json` | `rayap-bench freshness --pages 20000 --days 60 --warmup-days 20 --budget-per-page 0.2 --static-share S --log10-rate-min -2 --log10-rate-max 0 --seed K` |
| `freshness-budget{0.05,0.5,1.0}-seed1.json` | same with `--static-share 0.7 --budget-per-page B --seed 1` |

Fixed in all runs: 20,000 pages; changing pages draw log10(rate/day) uniformly from
[-2, 0] (one change per 100 days to one per day); revisit floor 1/30 per day; prior rate
0.2/day with weight 0.5; the adaptive policy re-plans once a day; results are measured
from day 20 to day 60 (days 0-20 let the adaptive policy observe).

## Batched commits, before/after (evening of 7 Nov 2026)

| File | Command |
|---|---|
| `batch-before-eb2db7a-run{1,2,3}.json` | `rayap-bench` built at `eb2db7a` (one transaction per page outcome): `e2e --hosts 10000 --delay-ms 2000 --concurrency 8192 --max-seconds 900 --sim-bin <simulator built at HEAD>` |
| `batch-after-run{1,2,3}.json` | the same command with `rayap-bench` built at HEAD (one transaction per drained batch, at most 512 outcomes) |

Runs alternated before 1, after 1, before 2, after 2, before 3, after 3, same simulator binary
and referee for all six. The machine was busier than on 6 Nov (load average 1.4–3.2; a
`rayap serve` process and other sessions were running), so absolute rates are lower than
the archived 6 Nov runs; only the before/after ratio is comparable. Referee: 0 proven
violations and 0 uncertain in all six runs (one `too_soon` uncertain in before-run2).

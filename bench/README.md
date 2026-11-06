# rayap benchmark

This measures how many pages per second rayap fetches, parses and records in SQLite while
staying polite, against a simulated web on one machine. It does not measure the public
internet (real DNS, TLS, bandwidth, server variety). Every number here comes from a file in
[`results/2026-11-06/`](results/2026-11-06/). [`RUN.md`](results/2026-11-06/RUN.md) lists
the exact command for each file.

## Setup

| | |
|---|---|
| Machine | Intel Core i7-12650H laptop, 16 logical CPUs, 13.7 GiB RAM, WSL2 (kernel 6.18.58). Not otherwise idle |
| Simulator | `rayap-sim`. 10,000 virtual hosts by default, each with its own loopback IP (127.1.0.1 …), robots.txt, a root page and 20 pages of 30 KiB HTML linked to each other |
| Trouble injected | log-normal latency (median 80 ms, sigma 1.0, so p99 about 10x the median); 2% of hosts 10x slower; 1% of pages 500 once; 0.5% 429 with `Retry-After: 5`; 1% 404; 1% redirect; 10% of hosts disallow `/private` (every page links there); 2% `Crawl-delay: 4`; 20% no robots.txt; 1% robots.txt 503 once |
| Crawler | `rayap-core` as shipped: 2 s per-host delay, depth 2, at most 8,192 requests in flight, every page outcome committed to SQLite (WAL, `synchronous=NORMAL`: survives a process crash, not necessarily a power loss) |
| Referee | the simulator counts violations itself, independent of the crawler |

**What the referee counts as a violation:**
- a second request to a host while one is in flight;
- a request sooner than the delay after the previous response's last body byte was
  written (Crawl-delay counts once a robots.txt with it has been served);
- a request before a `Retry-After` expired;
- a page requested before a usable robots.txt response: none yet, or the last one was a
  5xx or 429;
- a disallowed path.

A test (`referee_measures_gaps_from_the_end_of_the_body`) streams a slow body and checks
that an early follow-up request is caught. Another test checks that a page fetched after a
failed robots.txt is caught.

All traffic is plain HTTP over loopback. The simulator used about 0.2 CPU cores.

**Two windows are reported for every run:**
- **Steady:** the longest run of consecutive seconds at 50% of the peak rate or more.
  Before it, every host is fetching its robots.txt and root page. After it, only hosts with
  long Crawl-delays or pending retries remain.
- **Whole run:** from start to the last page.

Throughput, CPU and coordinator busy time are always measured over the same window.

## Results

### End to end, 10,000 hosts (3 runs, revision `f6a9321`)

| | run 1 | run 2 | run 3 |
|---|---|---|---|
| Pages fetched and recorded (200, parsed, committed) | 395,715 | 395,715 | 395,715 |
| **Steady window** | 63 s | 96 s | 99 s |
| pages/s | 3,940 | 3,999 | 3,881 |
| crawler CPU cores | 1.60 | 1.66 | 1.62 |
| coordinator busy | 81% | 86% | 87% |
| **Whole run** | 227 s | 226 s | 228 s |
| pages/s | 1,743 | 1,751 | 1,736 |
| crawler CPU cores | 0.76 | 0.75 | 0.74 |
| coordinator busy | 39% | 39% | 40% |
| Seconds to 90% of pages | 94 | 91 | 94 |
| Peak memory | 942 MiB | 914 MiB | 944 MiB |
| **Politeness violations (referee)** | **0** | **0** | **0** |

Each run made 419,622 requests (10,092 for robots.txt) and recorded 18,315 pages blocked
by robots.txt, 3,932 gone and 3,857 redirects. Politeness caps throughput at 10,000 hosts
÷ 2 s = 5,000 pages/s. In the steady window rayap reached 78–80% of that cap.

The whole-run rate is lower because of the tail. Once most hosts are done, the rest are
the slow ones: hosts with `Crawl-delay: 4`, 10x latency, or a retry pending. Those hosts
cannot go faster without breaking politeness.

For scale, 100 million pages a day is 1,157 pages/s. On this simulator, one rayap process
on a laptop sustained about 3.4 times that rate in the steady window and 1.5 times it over
the whole run. There were zero politeness violations. This does not show that rayap handles
100 million real pages a day (see Limits).

### One optimisation, measured

The first profile (`flamegraph-before.svg`) showed three costs on the coordinator, the one
task that also schedules requests:
- HTML extraction was 52% of all samples;
- entity decoding, which scanned each page's text six times, was 22% of all samples;
- whitespace collapsing was 24%;
- SQLite re-parsing the same SQL for every page was 7% (`sqlite3Prepare`).

The change, in commit `a861873`:
- HTML is parsed in the fetch tasks, which spread across cores;
- entity decoding is single-pass;
- hot SQL statements are prepared once;
- the per-host Crawl-delay lookup is O(1);
- the coordinator handles all finished responses before scheduling again.

Both versions ran against the same simulator and referee, with 4,096 requests in flight,
alternating:

| | before (`05a557a`) run 1 / 2 | after (`f6a9321`) run 1 / 2 |
|---|---|---|
| Wall time for the whole crawl | 286.6 / 284.2 s | 226.2 / 226.3 s |
| Whole-run pages/s | 1,381 / 1,393 | 1,750 / 1,748 |
| Seconds to 90% of pages | 184 / 182 | 90 / 91 |
| Crawler CPU seconds | 213 / 210 | 166 / 166 |
| Peak memory | 1,852 / 1,644 MiB | 914 / 923 MiB |
| Politeness violations | 0 / 0 | 0 / 0 |

With the change, 90% of pages are done in half the time, using 21% less CPU and about half
the memory. Whole-run throughput rose by 26%: the slow-host tail is mostly politeness
waiting, which the change does not and should not shorten.

In the new profile (`flamegraph-after.svg`), entity decoding is 0.7% of samples and
`sqlite3Prepare` is 0.4%. Extraction now runs in the fetch tasks. Whitespace collapsing
(31%) is still the largest single cost there, and the next thing to make cheaper.

### Scaling with host count (one run each, 8,192 in flight)

| Hosts | Politeness cap | Steady pages/s | % of cap | Steady coordinator busy | Steady CPU cores | Whole-run pages/s | Peak memory | Violations |
|---|---|---|---|---|---|---|---|---|
| 2,500 | 1,250 | 1,054 | 84% | 29% | 0.41 | 521 | 295 MiB | 0 |
| 5,000 | 2,500 | 2,084 | 83% | 50% | 0.81 | 1,032 | 617 MiB | 0 |
| 10,000 | 5,000 | 3,940 (median of 3) | 79% | 86% | 1.62 | 1,743 | 942 MiB | 0 |
| 20,000 | 10,000 | 5,163 | 52% | 100% | 2.40 | 2,687 | 2,391 MiB | 0 |
| 40,000 | 20,000 | 3,278 | 16% | 101%* | 1.85 | 2,573 | 4,593 MiB | 0 |

\* Busy time is measured in the coordinator and sampled once a second, so a sample can
round slightly above 100%.

### Scheduling core only

The frontier alone (no network, parsing or storage) handled 100,000 hosts × 50 pages with a
2 s delay and 200 ms requests. It made 5,000,000 scheduling decisions in 4.5 s, about 1.1
million per second, in 61 MiB (`scheduler-100k.json`). This measures the cost of choosing
what to fetch next. It is not a crawl rate.

## What breaks first

1. **The single coordinator.** One task schedules every request and commits every
   outcome. In the steady window it is busy 29% of the time at 2,500 hosts, 86% at 10,000
   and 100% at 20,000. Past that point, more hosts make it slower: 3,278 pages/s at
   40,000. Next steps:
   - commit outcomes in batches, one transaction per batch instead of per page;
   - shard hosts across several coordinators. Hosts share no state, so they split cleanly.
2. **Memory per URL queued.** Peak memory grows with the URLs known: 0.9 GiB at 10,000
   hosts, 4.6 GiB at 40,000. The frontier holds every pending URL as a string. A
   disk-backed frontier or interned URLs are needed long before a real-web crawl.
3. **One SQLite file.** It held about 4,000 page outcomes per second here, each in its own
   transaction (plus a small per-request host-hold write), with WAL and
   `synchronous=NORMAL`. Durable to a power cut (`synchronous=FULL`) was not measured.

## Limits

- **Not the internet.** The simulator answers over loopback with no TLS, no real DNS and
  no bandwidth limit. 30 KiB pages at 4,000 per second is about 1 Gbit/s, which a real
  deployment has to pay for.
- **The simulator is kind.** Pages are well-formed, nothing needs JavaScript, and hosts
  never ban the crawler. Latency follows a fixed distribution.
- **One laptop, WSL2.** Steady throughput moved about 3% across the three runs, and the
  steady window length varied (63 to 99 s). The machine was not otherwise idle.
- **Politeness sets the ceiling.** With a 2 s delay per host, 100 million pages a day needs
  about 2,300 hosts with work at every moment. Most of the real difficulty is the frontier:
  deciding which hosts and pages to keep busy.

## Reproduce

```sh
cargo build --release
# end to end (starts the simulator itself)
./target/release/rayap-bench e2e --hosts 10000 --delay-ms 2000 --concurrency 8192
# scheduling core only
./target/release/rayap-bench scheduler --hosts 100000 --pages-per-host 50
# with a CPU flamegraph
./target/release/rayap-bench e2e --hosts 10000 --flamegraph flamegraph.svg
# simulator options
./target/release/rayap-sim --help
```

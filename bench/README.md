# rayap benchmark

What this measures: how many pages per second rayap fetches, parses and durably records
while staying polite, against a simulated web on one machine. What it does not measure:
the public internet (real DNS, TLS, bandwidth, server variety). Every number below comes
from a JSON file in `results/2026-11-06/` and can be reproduced with the commands at the
end.

## Setup

| | |
|---|---|
| Machine | Intel Core i7-12650H laptop, 16 logical CPUs, 13.7 GiB RAM, WSL2 (kernel 6.18.58) |
| Simulator | `rayap-sim`: 10,000 virtual hosts by default, each with its own loopback IP (127.1.0.1 …), robots.txt, a root page and 20 pages of 30 KiB HTML linked to each other |
| Trouble injected | log-normal latency (median 80 ms, sigma 1.0, so p99 about 10x the median), 2% of hosts 10x slower, 1% of pages 500 once, 0.5% 429 with `Retry-After: 5`, 1% 404, 1% redirect, 10% of hosts disallow `/private` (every page links there), 2% `Crawl-delay: 4`, 20% no robots.txt, 1% robots.txt 503 once |
| Crawler | `rayap-core` as shipped: per-host delay 2 s, depth 2, SQLite state with every outcome committed, at most 8,192 requests in flight |
| Referee | the simulator counts violations itself, independent of the crawler: two requests in flight to one host, a request sooner than the delay after the previous response (Crawl-delay counted once robots.txt was served), a request before a `Retry-After` expired, a page before robots.txt, a disallowed path |

Plain HTTP over loopback. The simulator ran on the same machine and used about 0.2 CPU
cores. Steady state means the seconds at 50% of peak throughput or more. Before that
window, every host is fetching its robots.txt and root page. After it, only hosts with
long Crawl-delays or pending retries remain.

## Results

### End to end, 10,000 hosts (3 runs)

| | run 1 | run 2 | run 3 |
|---|---|---|---|
| Pages fetched and recorded (200, parsed, committed) | 395,715 | 395,715 | 395,715 |
| Steady-state pages/s, mean | 3,806 | 3,866 | 3,839 |
| Steady-state pages/s, median second | 3,924 | 3,935 | 3,937 |
| Wall time for the whole crawl | 227.8 s | 228.1 s | 228.2 s |
| Crawler CPU, average cores | 0.83 | 0.81 | 0.81 |
| Crawler peak memory | 905 MiB | 905 MiB | 975 MiB |
| Coordinator busy | 41% | 39% | 40% |
| **Politeness violations (referee)** | **0** | **0** | **0** |

Each run made 419,622 requests (10,092 of them for robots.txt) and saw 18,315 pages
blocked by robots.txt, 3,932 gone and 3,857 redirects. The upper bound set by politeness
is 10,000 hosts ÷ 2 s = 5,000 pages/s. rayap sustained about 77% of it.

For scale: 100 million pages a day is 1,157 pages/s. On this simulator, one rayap process
on a laptop sustained about 3.3 times that rate, with zero politeness violations. That
does not mean it handles 100 million real pages a day (see Limits).

### One optimisation, measured

The first profile (`flamegraph-before.svg`, from the run in
`e2e-10k-before-optimisation.json`) showed 48% of samples in HTML extraction. Within that,
20% went to an entity decoder that scanned each page's text six times, and 21% to
whitespace collapsing. A further 5% went to SQLite parsing the same SQL again for every
page. All of this ran on the one task that also schedules.

The change (commit `a861873`): parse pages in the fetch tasks, where the work spreads
across cores, and make entity decoding and whitespace collapsing single-pass. It also
prepares hot SQL statements once, makes the per-host Crawl-delay lookup O(1), and handles
all finished responses before scheduling again. Both binaries ran against the same
simulator binary, 4,096 requests in flight, alternating:

| | before (`05a557a`) | after (`a861873`) |
|---|---|---|
| Steady-state pages/s | 1,704 / 1,863 | 3,836 / 3,880 |
| Seconds to 90% of pages | 216 / 196 | 96 / 94 |
| Wall time | 310.7 / 296.1 s | 227.7 / 227.0 s |
| Crawler CPU seconds | 233 / 230 | 200 / 186 |
| Peak memory | 1,630 / 1,675 MiB | 956 / 892 MiB |
| Politeness violations | 0 / 0 | 0 / 0 |

That is about 2.2x the steady throughput, for less CPU and memory. In the new profile
(`flamegraph-after.svg`), entity decoding is 0.6% of samples and SQL parsing 0.4%.
Extraction now runs inside the fetch tasks, spread across cores. Whitespace collapsing is
still the largest single cost there, and it is the next thing to make cheaper.

### Scaling with host count (one run each, 8,192 in flight)

| Hosts | Politeness bound | Steady pages/s | % of bound | Coordinator busy | Crawler cores | Peak memory | Violations |
|---|---|---|---|---|---|---|---|
| 2,500 | 1,250 | 1,059 | 85% | 15% | 0.23 | 322 MiB | 0 |
| 5,000 | 2,500 | 2,109 | 84% | 26% | 0.45 | 584 MiB | 0 |
| 10,000 | 5,000 | 3,839 (median of 3) | 77% | 40% | 0.81 | 905 MiB | 0 |
| 20,000 | 10,000 | 4,114 | 41% | 64% | 1.34 | 2,403 MiB | 0 |
| 40,000 | 20,000 | 3,011 | 15% | 89% | 1.66 | 4,628 MiB | 0 |

### Scheduling core only

With no network, parsing or storage, the frontier handled 100,000 hosts × 50 pages with a
2 s delay and 200 ms requests. It made 5,000,000 scheduling decisions in 6.1 s, about
821,000 per second, in 61 MiB (`scheduler-100k.json`). This is the cost of choosing what
to fetch next, not a crawl rate.

## What breaks first

1. **The single coordinator.** One task schedules every request and commits every
   outcome to SQLite. Up to 10,000 hosts it is idle more than half the time, and
   throughput tracks the politeness bound. At 20,000 hosts it is busy 64% of the time and
   throughput stops growing. At 40,000 it is busy 89% and throughput falls. Next steps:
   commit outcomes in batches (one transaction per batch, not per page), then shard hosts
   across several coordinators. Hosts never share state, so they split cleanly.
2. **Memory per page queued.** Peak memory grows with hosts × pages known: 0.9 GiB at
   10,000 hosts and 4.6 GiB at 40,000. Every pending URL is held as a string in the
   frontier. A disk-backed frontier, or interned URLs, would be needed well before a
   real-web crawl.
3. **One SQLite file.** It held at about 4,000 commits per second here with WAL and
   `synchronous=NORMAL`. A durable store with batching or sharding is the step after that.

## Limits

- **Not the internet.** The simulator answers over loopback with no TLS, no real DNS and
  no bandwidth limit. Sending 30 KiB pages at 3,800 per second is about 0.9 Gbit/s, which
  a real deployment has to pay for.
- **The simulator is kind.** Pages are well-formed, there is no JavaScript to render, and
  hosts never ban the crawler. Latency follows a fixed distribution.
- **One laptop, WSL2.** Numbers move a few percent between runs (see the three runs
  above). The machine was not otherwise idle.
- **The politeness bound dominates.** With a 2 s delay per host, crawling 100 million
  pages a day needs at least about 2,300 hosts with work at every moment. Most of the real
  difficulty is the frontier: which hosts and pages to keep busy.

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

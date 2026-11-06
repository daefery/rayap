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

**When a response ends:** the simulator serves HTTP/1.1 directly on TCP sockets. A
response ends when the client's TCP stack has acknowledged its last byte (the kernel send
queue, `TIOCOUTQ`, is empty), or when delivery fails. The referee cannot see that instant
exactly. It records a window that is guaranteed to contain it. The window starts just
before the write call that handed over the final byte, and moves up to just before each
check that still found unacknowledged bytes. It ends just after the first check that found
none. A property test with random stalls at both edges checks that the true moment is
always inside. The window was at most 5.2 ms wide in any run, and a few
microseconds on average. A request counts as a violation only if it is too soon even when
measured from the earliest possible end, with no extra tolerance. Requests that are too
soon only when measured from a later point in the window are reported as `uncertain`.
Uncertain is not a measured miss rate. It only says the referee cannot decide.

**What the referee counts as a violation:**
- a second request to a host while one is in flight;
- a request sooner than the delay after the previous response ended (Crawl-delay counts
  once a robots.txt with it has been delivered);
- a request before a `Retry-After` expired;
- a page requested before a usable robots.txt response was delivered: none yet, the last
  one was a 5xx or 429, or the client abandoned it;
- a disallowed path.

**Testing the referee.** Tests in `crates/rayap-sim` check that the referee catches
deliberate violations:
- a reader that stalls a large body while a second connection asks again;
- a client that receives the last write but has not yet acknowledged it;
- an abandoned robots.txt;
- a page after a robots.txt 503;
- the proven/uncertain split.

I removed each fix in turn, and each time the matching test failed.

**Negative control at full scale.** The same 10,000-host crawl, with rayap set to a delay
slightly shorter than the 2,000 ms the referee enforces:

| rayap delay | Requests | Proven too soon | Uncertain |
|---|---|---|---|
| 1,950 ms | 419,622 | 183,341 (44%) | 396 |
| 1,990 ms | 419,622 | 80,004 (19%) | 6,464 |
| 2,000 ms (the real runs below) | 419,622 | 0 | 0 |

A crawler 10 ms too fast is caught tens of thousands of times. One that keeps its delay is
not caught at all.

All traffic is plain HTTP over loopback. The simulator used about 0.15 CPU cores.

**Two windows are reported for every run:**
- **Steady:** the longest run of consecutive seconds at 50% of the peak rate or more.
  Before it, every host is fetching its robots.txt and root page. After it, only hosts with
  long Crawl-delays or pending retries remain.
- **Whole run:** from start to the last page.

Throughput, CPU and coordinator busy time are always measured over the same window.

## Results

### End to end, 10,000 hosts (3 runs, revision `268fb3d`)

| | run 1 | run 2 | run 3 |
|---|---|---|---|
| Pages fetched and recorded (200, parsed, committed) | 395,715 | 395,715 | 395,715 |
| **Steady window** | 91 s | 94 s | 94 s |
| pages/s | 4,084 | 4,072 | 4,064 |
| crawler CPU cores | 1.47 | 1.52 | 1.55 |
| coordinator busy (estimate) | 84% | 84% | 84% |
| **Whole run** | 228.7 s | 226.4 s | 225.7 s |
| pages/s | 1,730 | 1,748 | 1,753 |
| crawler CPU cores | 0.68 | 0.69 | 0.70 |
| coordinator busy (estimate) | 38% | 37% | 37% |
| Seconds to 90% of pages | 93 | 90 | 90 |
| Peak memory | 984 MiB | 946 MiB | 937 MiB |
| **Politeness violations (proven)** | **0** | **0** | **0** |
| Uncertain | 0 | 0 | 0 |

Each run made 419,622 requests (10,092 for robots.txt) and recorded 18,315 pages blocked
by robots.txt, 3,932 gone and 3,857 redirects. Politeness caps throughput at 10,000 hosts
÷ 2 s = 5,000 pages/s. In the steady window rayap reached 81% of that cap.

The whole-run rate is lower because of the tail. Once most hosts are done, the rest are
the slow ones: hosts with `Crawl-delay: 4`, 10x latency, or a retry pending. Those hosts
cannot go faster without breaking politeness.

For scale, 100 million pages a day is 1,157 pages/s. On this simulator, one rayap process
on a laptop sustained about 3.5 times that rate in the steady window and 1.5 times it over
the whole run, with zero proven politeness violations. This does not show that rayap
handles 100 million real pages a day (see Limits).

### One optimisation, measured

The first profile (`flamegraph-before.svg`) showed three costs on the coordinator, the one
task that also schedules requests:
- HTML extraction was 48% of all samples;
- entity decoding, which scanned each page's text six times, was 20% of all samples;
- whitespace collapsing was 22%;
- SQLite re-parsing the same SQL for every page was 6% (`sqlite3Prepare`).

The change, in commit `a861873`:
- HTML is parsed in the fetch tasks, which spread across cores;
- entity decoding is single-pass;
- hot SQL statements are prepared once;
- the per-host Crawl-delay lookup is O(1);
- the coordinator handles all finished responses before scheduling again.

Both versions ran against the same simulator and referee (built at `268fb3d`), with 4,096
requests in flight, alternating:

| | before (`05a557a`) run 1 / 2 | after (`268fb3d`) run 1 / 2 |
|---|---|---|
| Wall time for the whole crawl | 280.3 / 295.7 s | 228.7 / 225.3 s |
| Whole-run pages/s | 1,412 / 1,338 | 1,731 / 1,756 |
| Seconds to 90% of pages | 177 / 197 | 94 / 90 |
| Crawler CPU seconds | 197 / 198 | 157 / 168 |
| Peak memory | 1,598 / 1,626 MiB | 984 / 887 MiB |
| Politeness violations (proven) | 0 / 0 | 0 / 0 |

With the change, 90% of pages are done in half the time, using 18% less CPU and 42% less
memory. Whole-run throughput rose by 27%: the slow-host tail is mostly politeness waiting,
which the change does not and should not shorten.

In the new profile (`flamegraph-after.svg`), entity decoding is 0.7% of samples and
`sqlite3Prepare` 0.3%. Extraction now runs in the fetch tasks, spread across cores.
Whitespace collapsing (36%) is still the largest single cost there, and the next thing to
make cheaper.

### Scaling with host count (one run each, 8,192 in flight)

| Hosts | Politeness cap | Steady pages/s | % of cap | Steady window | Coordinator busy (est.) | Steady CPU cores | Whole-run pages/s | Peak memory | Violations (proven / uncertain) |
|---|---|---|---|---|---|---|---|---|---|
| 2,500 | 1,250 | 1,059 | 85% | 90 s | 31% | 0.45 | 520 | 301 MiB | 0 / 1 |
| 5,000 | 2,500 | 2,106 | 84% | 90 s | 50% | 0.78 | 1,028 | 616 MiB | 0 / 0 |
| 10,000 | 5,000 | 4,072 (median of 3) | 81% | 94 s | 84% | 1.52 | 1,748 | 946 MiB | 0 / 0 |
| 20,000 | 10,000 | 4,495 | 45% | 161 s | 99% | 1.87 | 2,696 | 2,405 MiB | 0 / 1 |
| 40,000 | 20,000 | 4,539 | 23% | 68 s | 99% | 2.09 | 3,155 | 4,604 MiB | 0 / 0 |

### Scheduling core only

The frontier alone (no network, parsing or storage) handled 100,000 hosts × 50 pages with a
2 s delay and 200 ms requests. It made 5,000,000 scheduling decisions in 3.6 s, about 1.4
million per second, in 61 MiB (`scheduler-100k.json`). Across today's runs this figure
varied between 0.7 and 1.4 million per second with machine load. This measures the cost of choosing
what to fetch next. It is not a crawl rate.

## What breaks first

1. **The single coordinator.** One task schedules every request and commits every
   outcome. In the steady window it is busy 31% of the time at 2,500 hosts, 84% at 10,000
   and 99% at 20,000. Past that point, more hosts add no throughput: about 4,500 pages/s at
   both 20,000 and 40,000 hosts, against caps of 10,000 and 20,000. Next steps:
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
- **Busy fractions are estimates.** The coordinator adds its busy time after each piece of
  work, and samples are taken once a second, so a window edge can be off by one piece of
  work.
- **The simulator is kind.** Pages are well-formed, nothing needs JavaScript, and hosts
  never ban the crawler. Latency follows a fixed distribution.
- **One laptop, WSL2.** Steady throughput moved 0.5% across the three 10,000-host runs.
  Earlier runs today on the same code path moved up to 3%, and the scheduler-only figure
  moved by 2x. The machine was not otherwise idle.
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
# negative control: crawler deliberately 10 ms too fast for the referee
./target/release/rayap-bench e2e --hosts 10000 --delay-ms 2000 --crawler-delay-ms 1990
# with a CPU flamegraph
./target/release/rayap-bench e2e --hosts 10000 --flamegraph flamegraph.svg
# simulator options
./target/release/rayap-sim --help
```

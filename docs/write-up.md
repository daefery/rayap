# Three things a crawler taught me in a week

*rayap is a polite, restartable web crawl engine in Rust that I built from 6 to 8 November
2026. This is what I would tell a crawl team about it in ten minutes. Every number links to
the file it came from.*

## Why build another crawler

I wanted two things: a crawler I could point at a list of grant pages and TikTok posts and
have it tell me only what changed, and evidence that I can build crawling infrastructure,
not just use it. Those pull in the same direction if the engine is the product and the
sources are the test. So the design was: one coordinator per group of hosts, one request in
flight per host with a delay between requests, robots.txt per RFC 9309 before any page, all
state in SQLite so a crash resumes instead of restarting, and one JSON event per page
outcome. Reused: tokio, reqwest, `lol_html`, SQLite. Written: the frontier, the scheduler,
the robots parser, the recovery, the revisit planner, the extractors, the benchmark, and the
referee.

The referee is the part I would show first.

## 1. Proving politeness instead of claiming it

A crawler's politeness claim is usually "we wait N seconds between requests". The crawler
measures its own waiting, so the claim is unfalsifiable from outside. rayap's benchmark
simulator ([`rayap-sim`](../crates/rayap-sim)) serves 10,000 virtual hosts and judges every
request itself, from the server's side, at the TCP level: a response ends when the client's
stack has acknowledged its last byte (the kernel send queue is empty), and the next request
to that host is a violation if it arrives sooner than the delay after that moment. The
referee cannot see the acknowledgement instant exactly, so it records a window guaranteed to
contain it, counts a violation only if the request is too early even against the earliest
possible end, and reports the rest as *uncertain*, never as clean.

Then the negative control: run the same crawl with rayap set to 1,990 ms against a referee
enforcing 2,000 ms. The referee catches it **92,057 times** out of 419,622 requests. At
2,000 ms: **0 proven violations** across four runs
([table](../bench/README.md#setup), files in
[`bench/results/2026-11-06/`](../bench/results/2026-11-06/)). A politeness claim with a
10-millisecond detection threshold and a negative control is a different kind of claim.

Throughput in the same runs: **4,030 pages/s** in the steady window, 1,730 over the whole
crawl including the tail of slow hosts, one process, one laptop, pages parsed and committed
to SQLite ([`e2e-10k-run{1..4}.json`](../bench/results/2026-11-06/)). 100 million pages a
day is 1,157/s. The simulator is loopback with no TLS and well-formed pages; the write-up in
[`bench/README.md`](../bench/README.md) lists what it does not model.

Two things then moved that number. One transaction per drained batch instead of one per
page: **1.46x** steady, three alternating before/after pairs
([table](../bench/README.md#a-second-optimisation-batched-commits-7-nov-2026)). And when
one coordinator was 99% busy at 20,000 hosts, splitting hosts across shards with one store
writer: **1.79x** at 8 shards on 10,000 hosts, and the 20,000-host cliff gone
([table](../bench/README.md#sharded-coordinators-8-nov-2026)). The scaling is sub-linear
and the profile says why: the shards now wait on the single writer. That is recorded as
"what breaks first" with the next three steps, rather than hidden behind the headline.

## 2. The planner that learns which pages change, and its honest multiplier

Given a budget of revisits, which pages get them? rayap estimates each page's change rate
from what a crawler can actually observe ("changed since last visit?", not how many times;
the naive changes-per-time ratio undercounts fast pages) and allocates the budget to
maximise expected freshness, after Cho and Garcia-Molina, with a floor so no page is
abandoned ([`revisit.rs`](../crates/rayap-core/src/revisit.rs)).

The benchmark gives 20,000 pages hidden Poisson change rates and compares three policies on
identical change histories: uniform round robin, the planner learning from its own visits,
and the same planner handed the true rates (the ceiling). At one visit per page per five
days with 70% of pages static: changes caught per 1,000 fetches **137 → 219 → 255**,
freshness **0.913 → 0.947 → 0.966**, mean staleness **75 h → 28.5 h → 15.6 h**
([table, 4 workloads × 3 seeds](../bench/README.md#revisit-scheduling-learned-change-rates-vs-uniform-polling-7-nov-2026)).

Before this was measured, my own planning memo assumed a 100x here. The measured gain is
1.2x to 1.85x, and the arithmetic caps it: with a monthly floor and a five-day budget there
is no room for an order of magnitude. I wrote that into the benchmark instead of adjusting
the workload until the number looked better, and I flagged the one configuration where the
learned policy "beats" the oracle on changes-per-fetch, because it does so by chasing
fast-changing pages and losing freshness everywhere else. Caught-per-fetch is easy to game;
freshness and staleness are the metrics.

## 3. What real sites did to the engine

The simulator is kind. Real sites were not, and three of them changed the code.

**A 200 is not content.** On the first real run, one of six grant pages came back as 3,375
bytes with 55 characters of visible text and `<div id="root"></div>`: a React shell. rayap
had recorded it as `new` with a title and a hash, and would have reported `unchanged`
forever while the actual page changed behind it. The fix is a validator between fetch and
store: a page with no readable text is an `error`, never fingerprinted; a TikTok post whose
embedded data says *deleted* is `gone`; a 44,103-byte page with an empty title and a
bot-check script is a *challenge*, which holds the host for five minutes and retries, like
a 429 the server forgot to send. That last one appeared on 6 to 9 of 118 requests after
~470 requests to one host in two hours; the validator refused every one and nothing wrong
was stored ([`bench/tiktok/revisit/`](../bench/tiktok/revisit/README.md)). For pages that
genuinely need JavaScript, there is now a browser route through the same queue and robots
check; it does not solve captchas or spoof anything, and a site that stays empty in a
browser stays an `error`.

**A list is a set.** Revisiting 118 TikTok posts nine hours after the first crawl, 205 of
250 comment pages reported `changed`. Checking the comment ids: **98 of the 205 had exactly
the same ids in a different order**. TikTok sorts comments by engagement and the order
shifts between requests. By the posts' own counts, 2 of 115 had actually gained a comment.
The fingerprint of a comment page is now the sorted set of (id, text), and a post carries a
separate *activity* fingerprint (its comment count) so the comment pages are refetched when
someone commented, even though the post itself is unchanged. Fingerprints carry a method
version, and a version change reports `rebaseline`, never `changed`: the grants database
reported 21 rebaselines and 0 false changes on the next upgrade, where the previous build
had reported 13.

**Chrome is not content either.** The stored text of a grant page began "Skip to content
Search Close Search Menu About About Alpha-Omega Grants…". Main-content extraction now
drops navigation, headers, footers, asides, inline SVG and screen-reader-only text, and
keeps only a page's marked `main` region when it has one. On the 21 grant pages that cut
stored text by 19% and the records now start with the call. It also exposed that one
funder's grants page is empty even in a browser, which is recorded as such, not papered
over.

## What rayap is and is not

It is an engine with a measured politeness proof, a measured revisit planner, a validator
that has refused real challenge pages, sharded coordinators, structured sources (page text,
TikTok posts and comments, company sites), Web Bot Auth request signing that reproduces the
IETF draft's test vector byte for byte, and a read-only dashboard over the same SQLite
file. 141 tests; the end-to-end suite runs at 1 and N shards.

It is not an index, not a search engine, not distributed across machines, and not a way
around anything a site has decided to block. The next ceilings are named with numbers in
[`bench/README.md`](../bench/README.md): the single store writer, memory per queued URL,
one SQLite file.

The repository is [github.com/daefery/rayap](https://github.com/daefery/rayap). Everything
above can be re-run from the commands in `bench/results/*/RUN.md`.

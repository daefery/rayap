# Engine reference

[Back to the README](../README.md) | [Benchmark evidence](../bench/README.md) | [Source guides](sources.md)

Rayap runs in one process. Host-sharded coordinators schedule work, fetch tasks read pages,
and one writer commits the results to SQLite. This page holds the detailed behavior and
code map behind the README's overview.

## Scheduling and recovery

**One request in flight per host.** The default delay between requests is 2 seconds.
Hosts wait in a min-heap keyed by the time they may next be contacted, so a slow or
rate-limited host does not hold up the others. `--shards N` splits hosts across coordinators
by host name. Each host belongs to one coordinator.

**Backoff and stored holds.** HTTP `429` and `503` hold the whole host for the full
`Retry-After`, expressed as seconds or an HTTP date. Network errors and other server
failures retry with growing backoff, up to `max_attempts`. Holds are persisted for restart
recovery. When the next work is beyond `max_wait_ms`, the run can end unfinished and resume
later.

**At-least-once outcomes.** An outcome, its event, its records and discovered pages are
committed together. Shards hand cross-host pages to each other after the writer acknowledges
the commit. An interrupted run resumes pages with no committed outcome.

SQLite uses WAL and `synchronous=NORMAL`. Process-crash recovery and power-loss durability
are different guarantees. See the [storage limits](../bench/README.md#limits).

## Robots policy and redirects

Robots.txt is fetched before any page on an origin. The parser implements RFC 9309 matching:

- Rules for the `rayap` product token take precedence over `*`
- Longest match wins, with allow winning ties
- `*` and `$` are supported, with percent-encoding normalization
- The first 500 KiB is parsed, with a cache lifetime of at most 24 hours
- `Crawl-delay` is honored per origin, capped by `max_crawl_delay_ms`

A robots `4xx` other than `429` permits access. A `429`, server error or network failure
keeps pages waiting while robots.txt is retried. Pages become `blocked` after
`max_attempts` failures. Robots redirects, up to 5 hops, are scheduled as requests too.

Robots rules are respected by default. An
[explicit origin override](sources.md#explicit-robots-overrides) can ignore a known
`Disallow`, but does not remove robots fetch failures, delays or host holds.

HTTP page redirects queue the target as its own page. That target receives its own robots
and politeness checks, and chains stop after 10 hops. Ordinary link discovery stays on the
source's host. Browser navigation has different limits, described below.

## Validation and change detection

An HTTP success status alone does not establish useful content. Empty application shells,
recognized challenge pages and deleted posts reported inside JSON receive their own
outcomes instead of being treated as successful content. A recognized verification page
can hold its host for five minutes before retrying.

Page fingerprints cover extracted title and body text, excluding scripts, styles and
markup. Fingerprints include a method version. When that method changes, the next outcome
is `rebaseline`, not a content change.

This is content detection, not proof that every requested field was retrieved. A login
prompt can contain readable text. Structured-source coverage and mutable engagement fields
need their own checks.

## Revisits

`rayap crawl --budget N` selects known pages by their learned change rates, in addition to
pages never fetched. The estimator uses "changed since last visit" observations, following
the Cho and Garcia-Molina rate-estimation approach. Continuation pages, such as the next
page of a list, are revisited when their parent changed or its activity count moved.

The [planner simulation](../bench/README.md#revisit-scheduling-learned-change-rates-vs-uniform-polling-7-nov-2026)
compares uniform polling, learned rates and an oracle that knows the true rates. On the
reported workloads it caught 1.2 to 1.85 times as many changes per fetch. At 70% static pages,
it closed about two thirds of the freshness gap to the oracle and reduced mean staleness
from 75 hours to 28.5 hours. Those results are simulated, not a measured public-web gain.

## Identity and browser fetching

Every request identifies the crawler:

```text
Mozilla/5.0 (compatible; rayap/0.1.0; +<your contact>)
```

A contact is required. Optional `[signing]` configuration adds Web Bot Auth headers using
RFC 9421 HTTP Message Signatures, Ed25519 and `tag="web-bot-auth"`. The signing tests
reproduce the draft's test vector. Keys are published through a JWK Set at
`/.well-known/http-message-signatures-directory`.

```sh
rayap key new rayap.key
rayap key directory -c rayap.toml
```

`render = true` uses headless Chromium or Chrome. Its initial navigation goes through the
host queue and robots policy. The resulting DOM goes through extraction and validation.
Browser redirects and subresource requests do not receive the scheduler's per-request
checks. The DOM-only renderer also synthesizes status `200` rather than returning the real
HTTP status and final URL. See [JavaScript pages](sources.md#javascript-pages).

Rayap does not solve CAPTCHAs, reuse login sessions or spoof browser fingerprints.

## Event output

The CLI prints JSON lines. Each event has an increasing `id`, so a consumer can save its
last processed id and continue with `rayap events --db crawl.db --after N`.

| Kind | Meaning |
|---|---|
| `new` | First successful content fetch |
| `changed` | Content fingerprint differs |
| `unchanged` | Content fingerprint matches |
| `rebaseline` | Fingerprint method changed |
| `redirect` | A redirect target was discovered |
| `gone` | The page is reported gone |
| `blocked` | Robots policy prevented a page request |
| `error` | The fetch or extraction did not produce usable content |

`--changes-only` prints `new` and `changed` events. It does not print errors, blocked pages,
redirects or rebaselines. Use an unfiltered crawl or the Events view to inspect those.

An illustrative event, using synthetic content:

```json
{
  "id": 1,
  "run": 1,
  "at": 1700000000,
  "source": "demo",
  "url": "https://example.test/releases/",
  "kind": "new",
  "status": 200,
  "title": "Example release notes",
  "records": 1
}
```

## Findings from real sites

These are aggregate observations. Raw captures are kept outside the repository.

- Of 205 changed TikTok comment pages, 98 contained the same comments in a different order.
  Set fingerprints addressed that noise. A 44 KB challenge page served with HTTP `200`
  was refused by the validator. [TikTok measurements](../bench/tiktok/README.md)
- Of 23 grant funders' pages, two were JavaScript shells and were reported as such.
  [Grant-page measurements](../bench/grants/README.md)

## Code map

| Path | Responsibility |
|---|---|
| [`frontier.rs`](../crates/rayap-core/src/frontier.rs) | Per-host queues and earliest-eligible-host heap |
| [`crawler.rs`](../crates/rayap-core/src/crawler.rs) | Coordinators, robots gate, retries and holds |
| [`writer.rs`](../crates/rayap-core/src/writer.rs) | Batched writes from the coordinators |
| [`robots.rs`](../crates/rayap-core/src/robots.rs) | RFC 9309 parser and matcher |
| [`store.rs`](../crates/rayap-core/src/store.rs) | SQLite state, records, events and run recovery |
| [`revisit.rs`](../crates/rayap-core/src/revisit.rs) | Change-rate estimation and revisit allocation |
| [`fetch.rs`](../crates/rayap-core/src/fetch.rs) | Fetch routes, extractors and validation |
| [`html.rs`](../crates/rayap-core/src/html.rs) | Main-content extraction with `lol_html` |
| [`tiktok.rs`](../crates/rayap-core/src/tiktok.rs), [`company.rs`](../crates/rayap-core/src/company.rs), [`jobs.rs`](../crates/rayap-core/src/jobs.rs), [`ats.rs`](../crates/rayap-core/src/ats.rs) | Structured-source extraction |
| [`render.rs`](../crates/rayap-core/src/render.rs) | Headless browser route |
| [`signing.rs`](../crates/rayap-core/src/signing.rs) | Web Bot Auth signing |
| [`tests/crawl.rs`](../crates/rayap-core/tests/crawl.rs) | End-to-end tests against a local server |
| [`rayap-cli`](../crates/rayap-cli) | CLI commands |
| [`rayap-serve`](../crates/rayap-serve) | Dashboard and JSON API |
| [`rayap-sim`](../crates/rayap-sim) | Simulated web and politeness referee |
| [`rayap-bench`](../crates/rayap-bench) | Benchmarks and profiling |

## Next work

These are proposed follow-ups, not shipped capabilities.

1. Measure useful new comments per KB for planned revisits versus fetching everything.
   Earlier comparison windows had no new comments in either arm.
2. Reduce writer cost, then evaluate storage per shard. The benchmark discussion also
   proposes earlier internal acknowledgements. Any durability change needs separate tests.
3. Add conditional requests where validators help. The original 7-source probe on
   7 November 2026 found one source sending a validator and returning `304`, so this was
   deferred. That probe does not establish coverage for today's source list.
4. Extend signing with signed-directory responses and `Accept-Signature` negotiation.

See [what breaks first](../bench/README.md#what-breaks-first) for the measured constraints.

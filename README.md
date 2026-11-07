# rayap

A polite, restartable web crawl engine in Rust.

rayap crawls a list of sources and writes one JSON event per page: `new`, `changed`,
`unchanged`, `redirect`, `gone`, `blocked` or `error`. Its state lives in SQLite, so an
interrupted crawl resumes where it stopped and the next run reports only what changed.

> Status: early. The engine core and CLI work and are tested.

## Measured

On a simulated web of 10,000 hosts with injected latency, errors, 429s, redirects and
robots.txt rules, one rayap process on a laptop (i7-12650H, WSL2) fetched, parsed and
committed to SQLite at:

- **about 4,030 pages/s in the steady window** (median of 4 runs; one run slowed by other
  load on the laptop is included and explained);
- **about 1,730 pages/s over the whole crawl** (about 228 s), including the tail of slow
  and Crawl-delayed hosts;
- with a 2 s per-host delay and **zero politeness violations** proven by an independent
  referee.

The referee judges each response's end at the TCP level, from when the client
acknowledges the last byte, and counts only violations it can prove. It catches a crawler
set 10 ms too fast 92,057 times in the same crawl.

100 million pages a day is 1,157 pages/s. This is a loopback simulation, not the public
internet. [bench/README.md](bench/README.md) has:
- the setup and every raw result with its command;
- negative controls;
- a before/after optimisation;
- a scaling table showing where one coordinator stops scaling (about 20,000 hosts);
- the limits.

**Revisit scheduling.** Given a fixed budget of revisits, rayap's planner
(`revisit.rs`) learns each page's change rate from its own visits and spends the budget
where changes happen. On 20,000 simulated pages with hidden change rates, one revisit per
page per 5 days, 70% of pages static: changes caught per 1,000 fetches 137 → 219
(**1.6x**; an oracle that knows the true rates gets 255), freshness 0.913 → 0.947 (oracle
0.966), mean staleness 75 h → 28.5 h. The gain is 1.2x to 1.85x across the workloads tried,
never an order of magnitude; the table and the reasons are in
[bench/README.md](bench/README.md#revisit-scheduling-learned-change-rates-vs-uniform-polling-7-nov-2026).
The planner is not yet wired into `rayap crawl`.

**Structured sources.** 118 public TikTok posts → 115 `post` and 4,072 `comment` records,
three deleted or under-review posts reported as `gone` rather than stored as content, 0
challenge pages, 2 s between requests ([bench/tiktok](bench/tiktok/README.md)).

## How it behaves

- **One request at a time per host**, with a configurable delay between requests
  (default 2 s). A slow or rate-limited host never holds up the others: hosts wait in a
  min-heap keyed by the time they may next be contacted.
- **robots.txt per RFC 9309**, with rayap's own parser: fetched before any page on an
  origin, cached at most 24 hours, first 500 KiB parsed. Groups for the `rayap` product token
  win over `*`; longest match wins, allow wins ties; `*` and `$` supported; percent-encoding
  is normalized, so `/%70rivate` is `/private`. A 4xx robots.txt allows everything; a 429,
  5xx or network failure keeps the origin's pages waiting and is retried with backoff, and
  only after `max_attempts` failures are the pages reported as blocked. Each robots.txt
  redirect hop (up to 5) is scheduled like any other request. Malformed or hostile rules
  never widen access. `Crawl-delay` is honoured per origin, capped by `max_crawl_delay_ms`.
- **Backs off when asked**: `429` and `503` hold the whole host for the full `Retry-After`
  (seconds or HTTP date); network errors and other 5xx retry with growing backoff, up to
  `max_attempts`. Every wait is stored, so a restart never contacts a host early. If the
  only remaining work is further away than `max_wait_ms` (default 10 minutes), the run ends
  unfinished and the next run resumes it.
- **Redirects are not followed blindly**: the target is queued as its own page, so it passes
  robots.txt and politeness checks for its own host. Chains stop after 10 hops, and links are
  only followed on the source's own host.
- **Identifies itself**: `Mozilla/5.0 (compatible; rayap/0.1.0; +<your contact>)`. A contact
  is required.
- **Public pages only**: no logins, forms or CAPTCHAs.
- **Change detection ignores noise**: the fingerprint covers the page title and body text, not
  scripts, styles or markup, so a new tracking snippet is not a change.
- **At-least-once and resumable**: each outcome, its event and the links it found are
  committed in one SQLite transaction. If the process stops, the next start resumes the run and fetches only the
  pages that have no outcome yet.

## Use

```sh
cargo build --release
./target/release/rayap check -c examples/lampalampa-grants.toml
./target/release/rayap crawl -c examples/lampalampa-grants.toml --db grants.db
./target/release/rayap crawl -c examples/lampalampa-grants.toml --db grants.db --changes-only
./target/release/rayap events --db grants.db --after 120   # replay stored events
```

Config:

```toml
[crawler]
contact = "https://example.org/about-our-bot"   # required
per_host_delay_ms = 5000

[[source]]
name = "ngs-grants"
url = "https://www.nationalgeographic.org/explore/grants-investments"
max_depth = 0   # links to follow on the same host; 0 = this page only
```

An event:

```json
{"id":14,"run":2,"at":1793940000,"source":"ngs-grants","url":"https://...","kind":"changed",
 "status":200,"title":"Grants and Investments","hash":"…","prev_hash":"…","bytes":140213}
```

`id` only increases, so a consumer can store the last id it processed and read on from there.

### Structured sources: TikTok posts

A source with `kind = "tiktok"` reads a public post and its comments into records instead
of a text fingerprint. The post page embeds its data as JSON; comments come from the
paginated comment endpoint on the same host. Every request still goes through the host
queue, robots.txt and the stored holds. A `200` is not trusted blindly: a page without the
embedded data (challenge screen, empty application shell) is an `error`, a deleted or
under-review post is `gone`, and neither is recorded as content.

```toml
[[source]]
name = "frogs"
url = "https://www.tiktok.com/@tiktok/video/7106594312292453675"
kind = "tiktok"
max_comments = 60   # top-level comments to collect, in pages of 20
```

```sh
./target/release/rayap crawl -c sources.toml --db tiktok.db
./target/release/rayap records --db tiktok.db --kind post
./target/release/rayap records --db tiktok.db --kind comment --after 0
```

A `post` record carries `author`, `text`, `hashtags`, `mentions`, `music`, `duration_s`,
`created_at` and `stats` (plays, likes, comments, shares, saves); a `comment` record carries
`post_id`, `comment_id`, `author`, `text`, `likes`, `replies`, `created_at`. The change
fingerprint covers ids and text only, so a like count moving is not a change. Records are
keyed by `(kind, key)` and replaced when fetched again.

Measured 7 Nov 2026: 118 public posts, 2 s between requests, no login or cookies. See
[bench/tiktok](bench/tiktok/README.md). TikTok's robots.txt allows `/@handle/video/` for
rayap's user agent; its terms of service forbid automated collection, so keep volume low
and use it only where you have the right to.

## Layout

| Path | What |
|---|---|
| `crates/rayap-core/src/frontier.rs` | per-host queues and the earliest-eligible-host heap |
| `crates/rayap-core/src/crawler.rs` | the crawl loop: scheduling, robots.txt gate, retries |
| `crates/rayap-core/src/robots.rs` | RFC 9309 parser and matcher |
| `crates/rayap-core/src/store.rs` | SQLite state, runs, resumable progress, events |
| `crates/rayap-core/src/html.rs` | streaming extraction (title, text, links, nofollow) with `lol_html` |
| `crates/rayap-core/src/tiktok.rs` | TikTok post and comment pages into `post` / `comment` records |
| `crates/rayap-core/tests/crawl.rs` | end-to-end tests against a local server |
| `crates/rayap-cli` | the `rayap` command |
| `crates/rayap-sim` | simulated web plus an independent politeness referee |
| `crates/rayap-bench` | end-to-end and scheduler-only benchmarks, with flamegraphs |

## Roadmap

1. Wire the revisit planner into `rayap crawl`: per-page due times from learned change
   rates, and for structured sources revisit the cheap page first (a TikTok comment page is
   66 KiB for up to 20 records; the post page is 428 KiB for one). Measure bytes per new
   record and new records per fetch against today's visit-everything baseline.
2. Batched commits and several coordinators sharded by host, to scale past one
   coordinator (see "What breaks first" in the benchmark).
3. Conditional requests (`If-None-Match`, `If-Modified-Since`). Deferred: of the 7 real
   sources in `examples/` and `bench/tiktok`, checked 7 Nov 2026, only one
   (andpurpose.world) sends a validator and answers 304; the rest send neither `ETag` nor
   `Last-Modified`, so conditional requests would save nothing there.
4. Signed requests (RFC 9421 HTTP Message Signatures, Web Bot Auth), so sites can verify rayap.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

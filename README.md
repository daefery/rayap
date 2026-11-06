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

- **about 3,900 pages/s in the steady window** (median 3,940 of 3 runs; windows of
  63–99 s);
- **about 1,740 pages/s over the whole crawl** (227 s), including the tail of slow and
  Crawl-delayed hosts;
- with a 2 s per-host delay and **zero politeness violations** counted by an independent
  referee.

100 million pages a day is 1,157 pages/s. This is a loopback simulation, not the public
internet. [bench/README.md](bench/README.md) has:
- the setup and every raw result with its command;
- a before/after optimisation;
- a scaling table showing where one coordinator stops scaling (about 20,000 hosts);
- the limits.

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

## Layout

| Path | What |
|---|---|
| `crates/rayap-core/src/frontier.rs` | per-host queues and the earliest-eligible-host heap |
| `crates/rayap-core/src/crawler.rs` | the crawl loop: scheduling, robots.txt gate, retries |
| `crates/rayap-core/src/robots.rs` | RFC 9309 parser and matcher |
| `crates/rayap-core/src/store.rs` | SQLite state, runs, resumable progress, events |
| `crates/rayap-core/src/html.rs` | streaming extraction (title, text, links, nofollow) with `lol_html` |
| `crates/rayap-core/tests/crawl.rs` | end-to-end tests against a local server |
| `crates/rayap-cli` | the `rayap` command |
| `crates/rayap-sim` | simulated web plus an independent politeness referee |
| `crates/rayap-bench` | end-to-end and scheduler-only benchmarks, with flamegraphs |

## Roadmap

1. Batched commits and several coordinators sharded by host, to scale past one
   coordinator (see "What breaks first" in the benchmark).
2. Conditional requests (`If-None-Match`, `If-Modified-Since`) and bytes per check.
3. Signed requests (RFC 9421 HTTP Message Signatures, Web Bot Auth), so sites can verify rayap.
4. Adaptive revisits: learn how often each page changes and spend requests where changes happen.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

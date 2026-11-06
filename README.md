# rayap

A polite, restartable web crawl engine in Rust.

rayap crawls a list of sources and writes one JSON event per page: `new`, `changed`,
`unchanged`, `redirect`, `gone`, `blocked` or `error`. Its state lives in SQLite, so an
interrupted crawl resumes where it stopped and the next run reports only what changed.

> Status: early. The engine core and CLI work and are tested. The benchmark against a
> simulated web is the next milestone; until it exists, this README makes no speed claims.

## How it behaves

- **One request at a time per host**, with a configurable delay between requests
  (default 2 s). A slow or rate-limited host never holds up the others: hosts wait in a
  min-heap keyed by the time they may next be contacted.
- **robots.txt per RFC 9309**: fetched before any page on an origin, cached at most 24 hours,
  first 500 KiB parsed. Rules for the `rayap` product token win over `*`. A 4xx robots.txt
  allows everything; a 5xx or network failure blocks the origin. `Crawl-delay` is honoured,
  capped by `max_crawl_delay_ms`.
- **Backs off when asked**: `429` and `503` wait for `Retry-After` (seconds or HTTP date);
  network errors and other 5xx retry with growing backoff, up to `max_attempts`.
- **Redirects are not followed blindly**: the target is queued as its own page, so it passes
  robots.txt and politeness checks for its own host.
- **Identifies itself**: `Mozilla/5.0 (compatible; rayap/0.1.0; +<your contact>)`. A contact
  is required.
- **Public pages only**: no logins, forms or CAPTCHAs.
- **Change detection ignores noise**: the fingerprint covers the page title and body text, not
  scripts, styles or markup, so a new tracking snippet is not a change.
- **At-least-once and resumable**: each outcome and its event are committed in one SQLite
  transaction. If the process stops, the next start resumes the run and fetches only the
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
| `crates/rayap-core/src/robots.rs` | RFC 9309 interpretation (parsing by `texting_robots`) |
| `crates/rayap-core/src/store.rs` | SQLite state, runs, resumable progress, events |
| `crates/rayap-core/src/html.rs` | streaming extraction (title, text, links, nofollow) with `lol_html` |
| `crates/rayap-core/tests/crawl.rs` | end-to-end tests against a local server |
| `crates/rayap-cli` | the `rayap` command |

## Roadmap

1. Simulated web of 10,000 hosts with injected latency, errors and slow bodies; a benchmark
   that reports scheduler-only and end-to-end pages/s separately, with hardware, assumptions
   and politeness violations.
2. Conditional requests (`If-None-Match`, `If-Modified-Since`) and bytes per check.
3. Signed requests (RFC 9421 HTTP Message Signatures, Web Bot Auth), so sites can verify rayap.
4. Adaptive revisits: learn how often each page changes and spend requests where changes happen.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

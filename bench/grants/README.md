# Grant pages: two crawls of `examples/ai-oss-grants.toml` (7 Nov 2026)

`grants-run1.jsonl` is the first visit (all events), `grants-run2.jsonl` the second with
`--changes-only` (one line: the Simons URL corrected between runs). `*.stderr` are the
summary lines. 23 sources, 5 s per-host delay, rayap's identifying user agent, no login.

Run 1: 31 requests, 30 events, 3.0 MB. Run 2: 32 requests, 31 events, 3.3 MB.
Outcomes per run: 20 content pages; 6-7 redirects followed to content; prototypefund.de
403 (WAF; also 403 for curl with a browser user agent); www.mozillafoundation.org robots.txt
unreachable (TCP connects, no reply, 20-40 s, IPv4 and IPv6; foundation.mozilla.org answers
and redirects there), so the page is `blocked` per RFC 9309 rather than fetched.

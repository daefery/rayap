# Sources, records and the dashboard

What rayap can read beyond "fetch a page and fingerprint its text", and how to look at
what it stored. Everything here runs through the same engine: the host queue, robots.txt,
holds, the validator and the store are shared; a source kind only changes what is extracted.
Crawl output stays on the machine it ran on; nothing under `bench/` or `examples/` publishes
captured content.

## Dashboard

```sh
./target/release/rayap serve --db grants=grants.db --db tiktok=tiktok.db
# http://127.0.0.1:8732/
```

One read-only page over any number of crawl databases, each a *collection* (`grants`,
`tiktok`, anything you crawl next). Three views per collection, searchable: **Pages** (every
known page, its last outcome, when it was fetched, how often it changed), **Records** (what
was extracted: the visible text of each page, TikTok posts and comments; filter by kind,
search the text), **Events** (the change log). The same data as JSON:
`/api/collections`, `/api/{c}/pages?q=`, `/api/{c}/records?q=&kind=`, `/api/{c}/events?after=`.
The server only reads; `rayap crawl` can write to the same file meanwhile.

Every HTML page's main-content text is stored as a `page` record (up to 200 KB, marked
`truncated` beyond that), so a `changed` event can be read, not only noticed. Site chrome
(navigation, header, footer, asides, inline SVG) is left out, and a page that marks a
`main` or `article` region keeps only that; on the 21 grant pages this cut stored text by
19% and the records now start with the call, not the menu. A page with no readable text
at all (a JavaScript application shell) is an `error`, never fingerprinted; two of the real
example sources turned out to be such shells. An `unchanged` page writes no records.

## Watching grant pages

`examples/ai-oss-grants.toml` lists 23 public pages of funders that give grants to AI and
open-source projects (Sovereign Tech, NLnet, OTF, GitHub Secure Open Source Fund, Mozilla,
Humanity AI, Cooperative AI, NSF Safe-OSE, and others), 5 s between requests. Two crawls
on 7 Nov 2026, about 3 MB and 3 minutes each:

| | Run 1 | Run 2, `--changes-only` |
|---|---|---|
| Pages with content | 20 `new` | 20 `unchanged`, 1 `new` (a corrected URL) |
| Redirects followed | 6 (moved URLs at FUTO, GitHub, NSF, WASP-HS) | 7 |
| Not available | prototypefund.de `error` HTTP 403 (a WAF rule: curl with a browser UA gets 403 too); mozillafoundation.org `blocked` (its robots.txt never answers from this network, so rayap does not fetch the page); one 404 from a wrong URL guess | same |

The second run printed one line. From here on, `--changes-only` shows only pages whose
visible text changed: a new call, a moved deadline, a closed programme.

**LinkedIn is not a source.** Its robots.txt is `User-agent: * Disallow: /` with a notice
that automated access needs written permission (`whitelist-crawl@linkedin.com`); search and
feed URLs redirect to login, company post pages return 403. rayap's robots.txt gate blocks
every LinkedIn URL before any page request, which is the correct behaviour. The routes that
exist are their crawl whitelist, the official APIs for a page you own, or a page you control
where people post the links.

## JavaScript pages

`render = true` on a source fetches its pages with a headless Chromium or Chrome found on
`PATH` (or `RAYAP_BROWSER`), lets the page's scripts run for a bounded virtual time, and
digests the resulting DOM with the same extractor and validator as an HTTP body. The
request still waits its turn in the host's queue, passes robots.txt, and carries rayap's
user agent. newsfuel.org's opportunity pages, a 55-character shell over HTTP, come back as
900 characters of text with the deadline in them.

What it does not do: solve captchas, spoof fingerprints, rotate identities or reuse
logins. A challenge page rendered in a browser is still a challenge page. And a site whose
main region is empty even in a browser (alpha-omega.dev/grants on 8 Nov 2026) is still an
`error`. The browser's own subresource requests are not scheduled by rayap, so this route
is for the handful of sources that need it, not for broad crawls. Without a browser
installed, rendered sources report an error instead of silently falling back to HTTP.

## Structured sources: companies

`kind = "company"` reads an organisation's public website: the homepage becomes one
`company` record (name, legal name and logo from JSON-LD `Organization` when present,
description from meta tags, type, the well-known pages it links, its official social
profiles, and its main text), and each well-known page it links (about, careers, contact,
blog/news, products) becomes a `company_page` record with that page's text. Revisits
report which of those changed: a new role on the careers page, a new post, a changed
description. `examples/companies.toml` has six; on 8 Nov 2026 it produced 5 `company`
and 15 `company_page` records in 22 requests (one blocked: mozillafoundation.org's
robots.txt does not answer from this network).

Organisation-level, public facts only. The extractor does not look for people, personal
email addresses or phone numbers, and it records social profile URLs without visiting them.

## Employer job boards (Greenhouse, Lever, Ashby)

`kind = "ats"` reads an employer's own job board through the public API its
applicant-tracking system publishes (Greenhouse, Lever, Ashby) into `job` records: title,
department, the posting's location text verbatim, employment type, salary where the board
states it, posted time, an excerpt, and the employer's own posting URL. A `company` source
finds that board without being told: the careers page links `job-boards.greenhouse.io/<slug>`,
`jobs.lever.co/<slug>`, `jobs.ashbyhq.com/<slug>` or a `grnh.se` short link whose redirect
names it, and rayap hands over to the board's API. A listing on an aggregator is a copy;
the employer's board is the original, and revisits report new and removed postings there.

`kind = "jobs"` reads aggregator feeds published for redistribution (Himalayas, Remote OK)
into the same record shape, with the attribution each board asks for. Remotive's feed is
supported but its robots.txt disallows `/api/*`, so rayap blocks it.

Where a posting's text states where it hires ("Countries: …", "we can hire in …", "must be
based in …"), that passage is kept on the record as `hiring_sentence`, and `eligible_from`
applies a stated rule for a configured country against it (falling back to the location
field): the record always says which rule fired, so the verdict can be read and disagreed
with. A vendor's not-remote flag excludes outright. The rule is a dozen lines in
`jobs.rs` with a test per case; it is a reading aid, not a promise about an employer.

8 Nov 2026, `examples/companies.toml`: two employers' boards reached from their own sites
without configuration (one on Ashby, one on Greenhouse via short links); the Greenhouse
postings each carried a hiring-countries list, and 59 of the Ashby postings were marked
on-site by the vendor. Crawl output stays local; counts only are published here.

## Structured sources: TikTok posts

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
fingerprint covers ids and text only (comment pages as a set, since TikTok reorders them),
so a like count moving or a reorder is not a change. Records are keyed by `(kind, key)` and
replaced when fetched again.

**Revisits.** `rayap crawl --budget N` visits every page never fetched plus the N known
pages most due by their learned change rate (`rayap rates` shows the rates). Comment pages
are continuation pages: they are fetched again only when their post is new or changed, or
when the post's comment count moved. So an unchanged post costs one request per revisit.
A verification page served instead of a post (HTTP 200, no data) holds the host for five
minutes and the page is retried; nothing is stored.

Measured 7 Nov 2026: 118 public posts, 2 s between requests, no login or cookies. First
visit and revisits, as aggregates, in [bench/tiktok](../bench/tiktok/README.md): a
visit-everything revisit costs 368 requests and 67 MB, 77% of it post pages; a
`--budget 30` revisit 30 requests and 13 MB.
New comments found per byte is not measured yet (no comments arrived in the windows tried).
TikTok's robots.txt allows `/@handle/video/` for rayap's user agent; its terms of service
forbid automated collection, so keep volume low and use it only where you have the right to.


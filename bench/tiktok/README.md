# TikTok posts and comments with rayap (7 Nov 2026)

118 public TikTok post URLs (`urls.txt`, found through web search across funny, cooking,
dance, tech, fitness, Jakarta food, science and pet topics; no account, no cookies, no
browser) crawled with `rayap crawl -c sources.toml`, `kind = "tiktok"`, `max_comments = 60`,
2 s between requests, rayap's identifying user agent.

## Result

| | |
|---|---|
| Events | 368: 365 `new`, 3 `gone` |
| Post pages | 118: 115 posts, 3 `gone` (HTTP 200 with `statusCode 10204`: deleted, "audit not pass", "reviewing"); none recorded as content |
| Comment pages | 250 (up to 3 per post, 20 comments each) |
| Records | 115 `post`, 4,072 `comment` from 4,038 distinct commenters |
| Bytes | 67.5 MB |
| Wall time | 2,147 s across two runs (see below) |
| Challenge pages | 0 |

Files: `posts.jsonl` and `comments.jsonl` are `rayap records --kind post|comment` output;
`events.jsonl` the crawl events; `tiktok-118.json` the same data with comments nested under
their post.

The first run was killed part-way (its terminal pane was closed) after 310 events. The
second invocation resumed run 1 with the 58 pending pages, made 58 requests, and produced no
duplicate records: records are keyed by `(kind, key)`.

## Cross-check against an independent probe

`tiktok-100.json` was produced earlier the same day by a 150-line Python script against the
first 100 of the same URLs (same two endpoints, same politeness). For the 100 posts in both:
0 differences in post text, author or hashtags; 3,373 comments in both with 0 text
differences. The two sets differ in which comments they hold (272 only in the probe, 699 only
in rayap) because TikTok orders comments by engagement and the crawls were an hour apart, and
rayap collected up to 60 per post where the probe stopped at 3 pages of 18-20.

## Limits

- Top-level comments only; replies need a second endpoint per comment.
- The comment endpoint (`/api/comment/list/`) is undocumented. When it stops answering with
  `status_code: 0`, rayap reports `error`, not content.
- TikTok's robots.txt allows `/@handle/video/` for rayap's user agent; its terms of service
  forbid automated collection. Low volume, public posts only, and only where you have the
  right to.
- `stats` (plays, likes) are a snapshot at fetch time and are not part of the change
  fingerprint.

# TikTok posts and comments with rayap (6–8 Nov 2026): aggregates

The raw captures that used to live here (post and comment records, event logs, the URL
lists) were removed from the repository and its history on 9 Nov 2026: they contained other
people's handles, user ids and comment text, and TikTok's terms of service forbid automated
collection even where robots.txt allows the path. The files are kept locally, outside the
repository. What follows is every number the write-ups cite, re-derived from those files
before removal, with no identifying data.

Setup for all runs: `kind = "tiktok"`, `max_comments = 60` (three API pages of 20), 2 s
between requests, rayap's identifying user agent, no login, no cookies, no browser. Posts
were public and found through web search across eight topics (funny, cooking, dance, tech,
fitness, Jakarta food, science, pets).

## First crawl (6/7 Nov, 23:00–00:30 UTC)

| | |
|---|---|
| Post URLs tried | 118 |
| Events | 368: 365 `new`, 3 `gone` |
| Post pages | 118: 115 posts from 110 distinct accounts; 3 `gone` (HTTP 200 whose embedded data said `statusCode 10204`: one deleted, one "audit not pass", one "reviewing"); none stored as content |
| Comment pages | 250 (up to 3 per post) |
| Records | 115 `post`, 4,072 `comment` from 4,038 distinct commenters |
| Posts with all their comments (≤ 60 in total) | 48 |
| Bytes | 67.5 MB (post pages median 428 KiB for one record; comment pages median 66 KiB for up to 20) |
| Wall time | 2,147 s across two invocations: the first was killed at 310 events; the second resumed the 58 pending pages and produced no duplicate record |
| Challenge pages | 0 |

**Cross-check.** An independent 150-line Python probe had fetched the first 100 of the same
URLs an hour earlier through the same two endpoints (322 requests, 777 s, 100 posts, 3,665
comments). For the 100 posts in both: 0 differences in post text, author handle or
hashtags; 3,373 comments in both with 0 text differences. The sets differ in which
comments they hold (272 only in the probe, 699 only in rayap) because TikTok orders
comments by engagement and the crawls were an hour apart, and rayap collected up to 60 per
post where the probe stopped at 3 pages of 18–20.

## Revisits (7 Nov)

Two copies of the first-crawl database were revisited the next morning with the same
sources. Times UTC.

| Run | Start | Requests | MB | Post pages | Comment pages |
|---|---|---|---|---|---|
| baseline-run2, visit everything | 08:16 | 368 | 67.3 | 118 (115 unchanged, 3 gone) | 250 (205 changed, 45 unchanged) |
| baseline-run3, visit everything, set fingerprint | 09:52 | 368 | 67.3 | 118 (115 unchanged, 3 gone) | 250 (235 changed, 15 unchanged) |
| planned-run2, `--budget 30` | 08:30 | 30 | 13.1 | 30 (29 unchanged, 1 gone) | 0 |
| planned-run3, `--budget 30` | 11:36 | 30 | 12.7 | 30 (27 unchanged, 2 gone, 1 error) | 0 |
| planned-run4, `--budget 118` | 11:38 | 118 | 48.3 | 118 (107 unchanged, 2 gone, 9 error) | 0 |
| planned-run5, `--budget 118` | 11:47 | 118 | 48.3 | 118 (106 unchanged, 3 gone, 9 error) | 0 |
| planned-run6, `--budget 118` | 11:55 | 118 | 49.4 | 118 (109 unchanged, 3 gone, 6 error) | 0 |

The code moved during the morning (commits `2c6002a`, `3e09858`, `b0a7fb2`, `0541d7f`), so
these are not one clean experiment. What each run established:

1. **The first comment-page fingerprint was wrong (baseline-run2).** 205 of 250 comment
   pages reported `changed` nine hours after the first visit. Comparing comment ids: **98
   of the 205 had exactly the same ids** in a different order (TikTok sorts comments by
   engagement; the order shifts between requests). Of the rest, most "new" ids were old
   comments drifting into the top 20. By the posts' own `stats.comments`, **2 of 115 posts**
   had actually gained a comment. Fix (`3e09858`): a comment page is fingerprinted as the
   sorted set of (id, text). baseline-run3 is the first run with the new fingerprint, so its
   235 `changed` are a rebaseline against the old hashes, not changes; an explicit
   `rebaseline` event was added afterwards (`277b33f`) so this cannot recur.
2. **A post revisit alone cannot see new comments (planned-run2, planned-run3).** With
   `--budget 30` the planner visited the 30 most-due post pages, found them unchanged (the
   post fingerprint deliberately excludes counts) and, correctly, fetched no comment page:
   13 MB and 30 requests instead of 67 MB and 368, but blind to new comments. Fix
   (`3e09858`): a post carries a separate activity fingerprint (its comment count); comment
   pages are refetched when it moves even though the post is `unchanged`. planned-run4 to 6
   are the first readings of that fingerprint, so no comparison was possible within them,
   and in the 20 minutes they span no post's count moved.
3. **The host pushed back (planned-run3 to 6).** The `error` rows are a 44,103-byte
   verification page: HTTP 200, empty `<title>`, TikTok's `webmssdk` bot-check script, a
   captcha container, no embedded data. It appeared only in the planned arm, which had made
   five passes over the same host in two hours (about 470 requests at a 2 s delay); the
   baseline arm, visited 1.5–3 h apart, never saw one. The same URLs answered normally a
   minute later. The validator refused every one (nothing stored, last good content and
   hash kept). Fix (`0541d7f`): the page is classified as a challenge and treated like a 429
   without `Retry-After`: the host is held for five minutes and the page retried, up to
   `max_attempts`.

Cost of a visit-everything revisit: 368 requests, 67.3 MB, 77% of it the 118 post pages
(about 430 KB of HTML around a 26 KB JSON blob each). A `--budget 30` revisit: 30 requests,
12.7–13.1 MB. **Not measured:** new comments found per KB, which these runs were for. In
the 3-hour windows available, on months-old posts, essentially no new comments arrived in
either arm.

## Experiment 2 (set up 7 Nov, run locally)

49 posts from TikTok's own public daily trend pages (`/q/trends/tiktok-general-trends-us-
<date>`, 27 Oct to 5 Nov 2026), days old and still receiving comments. First visit: 49
post pages, 46 `new`, 3 `gone`, 98 comment pages, 1,503 comments, 27.2 MB. Two arms
(visit-everything vs `--budget 15`) run once a day by a local cron job that snapshots each
arm's comment ids, so "new since yesterday" is exact. The scripts and results stay local
with the raw data; a result, when there is one, will be reported here as counts only.

## Limits

- Top-level comments only; replies need a second endpoint per comment.
- The comment endpoint (`/api/comment/list/`) is undocumented. When it stops answering with
  `status_code: 0`, rayap reports `error`, not content.
- TikTok's robots.txt allows `/@handle/video/` for rayap's user agent; its terms of service
  forbid automated collection. The crawls above were low-volume and public-only, and the
  captured data is not published.
- `stats` (plays, likes) are a snapshot at fetch time and are not part of the change
  fingerprint.

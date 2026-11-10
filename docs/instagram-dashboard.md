# Instagram capture records in the dashboard

The Records view can display `instagram_post` and `instagram_comment` records imported
from saved public-page captures. This is display support, not a built-in Instagram source
or a guarantee of complete post/comment retrieval. Keep captures and databases outside Git.

Add the local collection when starting the dashboard, alongside any existing collections:

```sh
./target/debug/rayap serve --db instagram="$HOME/rayap/instagram.db" --listen 127.0.0.1:8732
# Open http://127.0.0.1:8732/#instagram/records
```

Use the record-kind selector to show posts or individual comments. Search matches stored
record data, including captions, comment text and usernames. Post records include an
expanded, scrollable list of captured comments; separate comment records make those same
comments individually searchable.

## What is shown

- Post owner, caption, source link, publication time and capture time.
- Likes, displayed comment total and shares **as observed labels**. An abbreviation is
  not converted into a precise number; absent values display as `unknown`, never zero.
- Captured comment usernames, text, timestamps, age labels at capture, any displayed
  comment-like counts, comment permalinks and links back to the post.
- A partial-capture warning and whether comment pagination was attempted. A page saying
  “No comments yet.” is recorded as an observation, not proof of complete coverage.

The original generic `page` records can remain in the collection for comparison. Offline
imports should preserve the original capture timestamps and annotate derived event details;
they must not claim a new network fetch. Re-running an ordinary page crawl does not itself
refresh these structured snapshot records.

## Record data contract

Post data uses `username`, `text`, `post_id`, `url`, `created_at` (Unix seconds),
`stats_display` (`likes`, `comments`, `shares`: observed strings or null), `comments`,
`comments_status` (`partial` or `no_comments_shown`), and `capture` (`method`,
`pagination_attempted`). Current display support treats comment coverage as unverified.

Each comment uses `id` (a string, not a floating-point number), `username`, `text`,
`created_at`, `age_label`, `likes_display` (string or null), `url` (comment permalink),
and `post_url`. An `instagram_comment` record has that same object as its data. Its key
should include the parent post identity and comment identity. Post records store the
comment objects under `comments`; generate both forms from the same parsed capture.

Renderers escape text and labels and only create links for HTTP(S) URLs. They do not load
remote images or scripts from the captures.

## Tests

The renderer tests use synthetic records and Node's built-in test runner; no npm install
or real captured data is needed:

```sh
node --test crates/rayap-serve/tests/dashboard.test.cjs
cargo test -p rayap-serve
```

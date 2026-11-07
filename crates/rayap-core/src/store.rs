//! Durable crawl state in SQLite.
//!
//! A *run* visits every known page once. A page is pending in run R until its outcome is
//! committed with `done_run = R`. The outcome and its event are written in one transaction,
//! so after a crash the page is simply fetched again: delivery is at-least-once, and an
//! unfinished run is resumed instead of starting over.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Serialize;

use crate::Result;
use crate::revisit::{self, Observation, Prior};

const SECONDS_PER_DAY: f64 = 86_400.0;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS runs (
    id          INTEGER PRIMARY KEY,
    started_at  INTEGER NOT NULL,
    finished_at INTEGER
);
CREATE TABLE IF NOT EXISTS pages (
    url           TEXT PRIMARY KEY,
    source        TEXT NOT NULL,
    depth         INTEGER NOT NULL,
    hops          INTEGER NOT NULL DEFAULT 0,
    queued_run    INTEGER NOT NULL,
    done_run      INTEGER,
    attempts      INTEGER NOT NULL DEFAULT 0,
    status        INTEGER,
    etag          TEXT,
    last_modified TEXT,
    content_hash  TEXT,
    title         TEXT,
    fetched_at    INTEGER,
    content_at    INTEGER,
    continuation  INTEGER NOT NULL DEFAULT 0,
    activity_hash TEXT
);
CREATE INDEX IF NOT EXISTS pages_pending ON pages (queued_run, done_run);
CREATE TABLE IF NOT EXISTS revisits (
    url           TEXT NOT NULL,
    at            INTEGER NOT NULL,
    interval_days REAL NOT NULL,
    changed       INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS revisits_url ON revisits (url, at);
CREATE TABLE IF NOT EXISTS robots_failures (
    origin   TEXT PRIMARY KEY,
    run      INTEGER NOT NULL,
    failures INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS host_holds (
    host       TEXT PRIMARY KEY,
    until_ms   INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
    id        INTEGER PRIMARY KEY,
    run       INTEGER NOT NULL,
    at        INTEGER NOT NULL,
    source    TEXT NOT NULL,
    url       TEXT NOT NULL,
    kind      TEXT NOT NULL,
    status    INTEGER,
    title     TEXT,
    hash      TEXT,
    prev_hash TEXT,
    bytes     INTEGER,
    detail    TEXT,
    records   INTEGER
);
CREATE TABLE IF NOT EXISTS records (
    id      INTEGER PRIMARY KEY,
    run     INTEGER NOT NULL,
    at      INTEGER NOT NULL,
    source  TEXT NOT NULL,
    url     TEXT NOT NULL,
    kind    TEXT NOT NULL,
    key     TEXT NOT NULL,
    data    TEXT NOT NULL,
    UNIQUE (kind, key)
);
";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EventKind {
    /// First successful fetch of this URL.
    New,
    /// Content differs from the previous successful fetch.
    Changed,
    /// Content is the same as the previous successful fetch.
    Unchanged,
    /// The server redirected; `detail` holds the target, which is crawled as its own page.
    Redirect,
    /// 404 or 410.
    Gone,
    /// Not fetched because robots.txt disallows it or could not be read.
    Blocked,
    /// Gave up after retries, or the response could not be used.
    Error,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::New => "new",
            EventKind::Changed => "changed",
            EventKind::Unchanged => "unchanged",
            EventKind::Redirect => "redirect",
            EventKind::Gone => "gone",
            EventKind::Blocked => "blocked",
            EventKind::Error => "error",
        }
    }

    fn parse(s: &str) -> EventKind {
        match s {
            "new" => EventKind::New,
            "changed" => EventKind::Changed,
            "unchanged" => EventKind::Unchanged,
            "redirect" => EventKind::Redirect,
            "gone" => EventKind::Gone,
            "blocked" => EventKind::Blocked,
            _ => EventKind::Error,
        }
    }
}

/// One line of rayap's output. `id` increases monotonically, so a consumer can store the
/// last id it processed and ask for everything after it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Event {
    pub id: i64,
    pub run: i64,
    /// Unix seconds.
    pub at: i64,
    pub source: String,
    pub url: String,
    pub kind: EventKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prev_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Structured records (posts, comments) the page yielded, when the source extracts any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub records: Option<i64>,
}

/// A structured item extracted from a page (a post, a comment). `kind` and `key` identify it
/// across runs: fetching it again replaces `data`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRecord {
    pub kind: String,
    pub key: String,
    pub data: serde_json::Value,
}

/// A stored record, as `rayap records` prints it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Record {
    pub id: i64,
    pub run: i64,
    /// Unix seconds of the fetch that last produced it.
    pub at: i64,
    pub source: String,
    pub url: String,
    pub kind: String,
    pub key: String,
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPage {
    pub url: String,
    pub source: String,
    pub depth: u32,
    /// Redirects followed to reach this URL.
    pub hops: u32,
    pub attempts: u32,
}

/// A URL found while handling a page (a link or a redirect target).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPage {
    pub url: String,
    pub source: String,
    pub depth: u32,
    pub hops: u32,
    /// Part of another page (the next page of a comment list): fetched when that page is new
    /// or changed, never planned on its own.
    pub continuation: bool,
}

/// How a new run chooses which known pages to visit. Without a plan, every known page of a
/// configured source is visited each run.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// Pages to visit this run, beyond never-fetched ones and whatever the visited pages lead to.
    pub budget: usize,
    /// Every page is visited at least this often, however rarely it seems to change.
    pub max_interval_days: f64,
    pub prior: Prior,
}

/// What `begin_run` decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunStart {
    pub run: i64,
    /// An unfinished run was picked up; nothing was re-planned.
    pub resumed: bool,
    /// Known pages left for a later run by the plan.
    pub deferred: u64,
}

/// What a fetch produced, ready to be committed.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    pub status: Option<u16>,
    pub title: Option<String>,
    /// Fingerprint of the content; `None` when there is no content (errors, redirects, blocks).
    pub hash: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub bytes: Option<i64>,
    pub detail: Option<String>,
    /// Committed with the event; empty for sources that only fingerprint pages.
    pub records: Vec<NewRecord>,
    /// Fingerprint of what makes the page's continuation pages worth refetching, when the
    /// source separates that from the content fingerprint.
    pub activity: Option<String>,
}

pub struct Store {
    conn: Connection,
}

pub fn unix_now() -> i64 {
    unix_now_ms() / 1000
}

pub fn unix_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let tx = conn.transaction()?;
        migrate(&tx)?;
        tx.commit()?;
        Ok(Self { conn })
    }

    /// Returns the unfinished run if there is one (resume), otherwise starts a new run and
    /// queues known pages of the configured sources: all of them, or, with a plan, the
    /// never-fetched ones plus the `budget` most due by learned change rate.
    pub fn begin_run(&mut self, sources: &[&str], plan: Option<&Plan>) -> Result<RunStart> {
        let open: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM runs WHERE finished_at IS NULL ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(run) = open {
            return Ok(RunStart {
                run,
                resumed: true,
                deferred: 0,
            });
        }
        let tx = self.conn.transaction()?;
        tx.execute("INSERT INTO runs (started_at) VALUES (?1)", [unix_now()])?;
        let run = tx.last_insert_rowid();
        let deferred = match plan {
            None => {
                let mut requeue =
                    tx.prepare("UPDATE pages SET queued_run = ?1, attempts = 0 WHERE source = ?2")?;
                for source in sources {
                    requeue.execute(params![run, source])?;
                }
                0
            }
            Some(plan) => {
                let chosen = choose_due(&tx, sources, plan, unix_now())?;
                let mut queue =
                    tx.prepare("UPDATE pages SET queued_run = ?1, attempts = 0 WHERE url = ?2")?;
                for url in &chosen.urls {
                    queue.execute(params![run, url])?;
                }
                chosen.deferred
            }
        };
        tx.commit()?;
        Ok(RunStart {
            run,
            resumed: false,
            deferred,
        })
    }

    /// Adds a page to run `run` only if it is not known at all. Returns true if it was added.
    pub fn enqueue_if_unknown(&self, run: i64, page: &NewPage) -> Result<bool> {
        let changed = self
            .conn
            .prepare_cached(
                "INSERT INTO pages (url, source, depth, hops, continuation, queued_run)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT (url) DO NOTHING",
            )?
            .execute(params![
                page.url,
                page.source,
                page.depth,
                page.hops,
                page.continuation,
                run
            ])?;
        Ok(changed > 0)
    }

    pub fn finish_run(&self, run: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET finished_at = ?1 WHERE id = ?2",
            params![unix_now(), run],
        )?;
        Ok(())
    }

    /// Adds a page to run `run`. Returns true if it was not already part of the run, in which
    /// case the caller must put it in the frontier.
    pub fn enqueue(&self, run: i64, page: &NewPage) -> Result<bool> {
        enqueue_in(&self.conn, run, page)
    }

    pub fn pending(&self, run: i64) -> Result<Vec<PendingPage>> {
        let mut stmt = self.conn.prepare(
            "SELECT url, source, depth, hops, attempts FROM pages
             WHERE queued_run = ?1 AND (done_run IS NULL OR done_run < ?1) ORDER BY rowid",
        )?;
        let rows = stmt.query_map([run], |r| {
            Ok(PendingPage {
                url: r.get(0)?,
                source: r.get(1)?,
                depth: r.get(2)?,
                hops: r.get(3)?,
                attempts: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Records that a retryable attempt failed, so a restart does not reset the count.
    pub fn record_attempt(&self, url: &str, attempts: u32) -> Result<()> {
        self.conn
            .prepare_cached("UPDATE pages SET attempts = ?1 WHERE url = ?2")?
            .execute(params![attempts, url])?;
        Ok(())
    }

    /// Commits the page outcome, its event and the pages it led to, in one transaction.
    /// Returns the event and the discovered pages that are new to this run (to be put in the
    /// frontier). For content outcomes (`hash` is set) the kind is decided here from the
    /// stored hash: `New`, `Changed` or `Unchanged`. Other kinds are taken as given.
    /// `continuation` pages are queued only when the outcome is `New` or `Changed`, or when
    /// the outcome's `activity` differs from the stored one.
    pub fn complete(
        &mut self,
        run: i64,
        url: &str,
        kind: Option<EventKind>,
        outcome: Outcome,
        discovered: Vec<NewPage>,
        continuation: Vec<NewPage>,
    ) -> Result<(Event, Vec<NewPage>)> {
        let tx = self.conn.transaction()?;
        let (event, activity_changed) = complete_in(&tx, run, url, kind, outcome)?;
        let mut added = Vec::new();
        let follow = matches!(event.kind, EventKind::New | EventKind::Changed) || activity_changed;
        let continuation = if follow { continuation } else { Vec::new() };
        for page in discovered.into_iter().chain(continuation) {
            if enqueue_in(&tx, run, &page)? {
                added.push(page);
            }
        }
        tx.commit()?;
        Ok((event, added))
    }

    /// Learned change rate (per day) of each known page of `source`, for inspection.
    pub fn rates(&self, source: &str, prior: Prior) -> Result<Vec<(String, f64)>> {
        let observations = observations_in(&self.conn)?;
        let mut stmt = self.conn.prepare(
            "SELECT url FROM pages WHERE source = ?1 AND continuation = 0 ORDER BY rowid",
        )?;
        let rows = stmt.query_map([source], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for url in rows {
            let url = url?;
            let obs = observations
                .get(&url)
                .map(Vec::as_slice)
                .unwrap_or_default();
            out.push((url, revisit::estimate_rate(obs, prior)));
        }
        Ok(out)
    }

    /// Persists that `host` must not be contacted before `until_ms` (Unix milliseconds).
    /// An existing later hold is kept.
    pub fn hold_host(&self, host: &str, until_ms: i64) -> Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO host_holds (host, until_ms) VALUES (?1, ?2)
                 ON CONFLICT (host) DO UPDATE SET until_ms = max(until_ms, excluded.until_ms)",
            )?
            .execute(params![host, until_ms])?;
        Ok(())
    }

    pub fn record_robots_failure(&self, run: i64, origin: &str, failures: u32) -> Result<()> {
        self.conn.execute(
            "INSERT INTO robots_failures (origin, run, failures) VALUES (?1, ?2, ?3)
             ON CONFLICT (origin) DO UPDATE SET run = excluded.run, failures = excluded.failures",
            params![origin, run, failures],
        )?;
        Ok(())
    }

    pub fn clear_robots_failures(&self, run: i64, origin: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM robots_failures WHERE origin = ?1 AND run = ?2",
            params![origin, run],
        )?;
        Ok(())
    }

    /// robots.txt failure counts recorded in `run`, as (origin, failures).
    pub fn robots_failures(&self, run: i64) -> Result<Vec<(String, u32)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT origin, failures FROM robots_failures WHERE run = ?1")?;
        let rows = stmt.query_map([run], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Holds that have not expired yet, as (host, until_ms).
    pub fn active_holds(&self) -> Result<Vec<(String, i64)>> {
        self.conn.execute(
            "DELETE FROM host_holds WHERE until_ms <= ?1",
            [unix_now_ms()],
        )?;
        let mut stmt = self.conn.prepare("SELECT host, until_ms FROM host_holds")?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Records with `id > after`, oldest first, optionally of one kind.
    pub fn records_after(
        &self,
        after: i64,
        kind: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Record>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, run, at, source, url, kind, key, data FROM records
             WHERE id > ?1 AND (?2 IS NULL OR kind = ?2) ORDER BY id LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![after, kind, limit as i64], |r| {
            let data: String = r.get(7)?;
            Ok(Record {
                id: r.get(0)?,
                run: r.get(1)?,
                at: r.get(2)?,
                source: r.get(3)?,
                url: r.get(4)?,
                kind: r.get(5)?,
                key: r.get(6)?,
                data: serde_json::from_str(&data).unwrap_or(serde_json::Value::Null),
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Events with `id > after`, oldest first.
    pub fn events_after(&self, after: i64, limit: usize) -> Result<Vec<Event>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, run, at, source, url, kind, status, title, hash, prev_hash, bytes, detail, records
             FROM events WHERE id > ?1 ORDER BY id LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![after, limit as i64], |r| {
            Ok(Event {
                id: r.get(0)?,
                run: r.get(1)?,
                at: r.get(2)?,
                source: r.get(3)?,
                url: r.get(4)?,
                kind: EventKind::parse(&r.get::<_, String>(5)?),
                status: r.get(6)?,
                title: r.get(7)?,
                hash: r.get(8)?,
                prev_hash: r.get(9)?,
                bytes: r.get(10)?,
                detail: r.get(11)?,
                records: r.get(12)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
}

struct Chosen {
    urls: Vec<String>,
    deferred: u64,
}

/// The pages a planned run visits: every never-fetched page, then the `budget` pages that
/// are most overdue, where overdue is the time since the last fetch times the revisit
/// frequency the planner gives the page from its learned change rate.
fn choose_due(conn: &Connection, sources: &[&str], plan: &Plan, now: i64) -> Result<Chosen> {
    struct Candidate {
        url: String,
        elapsed_days: Option<f64>,
    }
    let observations = observations_in(conn)?;
    let mut candidates = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT url, coalesce(content_at, fetched_at) FROM pages
         WHERE source = ?1 AND continuation = 0 ORDER BY rowid",
    )?;
    for source in sources {
        let rows = stmt.query_map([source], |r| {
            Ok(Candidate {
                url: r.get(0)?,
                elapsed_days: r
                    .get::<_, Option<i64>>(1)?
                    .map(|at| (now - at).max(0) as f64 / SECONDS_PER_DAY),
            })
        })?;
        for c in rows {
            candidates.push(c?);
        }
    }
    let rates: Vec<f64> = candidates
        .iter()
        .map(|c| {
            let obs = observations
                .get(&c.url)
                .map(Vec::as_slice)
                .unwrap_or_default();
            revisit::estimate_rate(obs, plan.prior)
        })
        .collect();
    let weights = vec![1.0; rates.len()];
    let floor = 1.0 / plan.max_interval_days.max(1e-9);
    let frequencies = revisit::allocate(&rates, &weights, plan.budget as f64, floor);
    let mut order: Vec<(f64, usize)> = candidates
        .iter()
        .zip(&frequencies)
        .enumerate()
        .map(|(i, (c, f))| (c.elapsed_days.map_or(f64::INFINITY, |d| d * f), i))
        .collect();
    order.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    let mut urls = Vec::new();
    let mut spent = 0;
    for (priority, i) in order {
        let never_fetched = priority.is_infinite();
        if !never_fetched {
            if spent >= plan.budget {
                break;
            }
            spent += 1;
        }
        urls.push(candidates[i].url.clone());
    }
    let deferred = (candidates.len() - urls.len()) as u64;
    Ok(Chosen { urls, deferred })
}

/// Every page's revisit observations, newest first, at most 64 per page.
fn observations_in(conn: &Connection) -> Result<HashMap<String, Vec<Observation>>> {
    let mut stmt =
        conn.prepare("SELECT url, interval_days, changed FROM revisits ORDER BY url, at DESC")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            Observation {
                interval: r.get(1)?,
                changed: r.get::<_, i64>(2)? != 0,
            },
        ))
    })?;
    let mut out: HashMap<String, Vec<Observation>> = HashMap::new();
    for row in rows {
        let (url, obs) = row?;
        let list = out.entry(url).or_default();
        if list.len() < 64 {
            list.push(obs);
        }
    }
    Ok(out)
}

/// Schema version stored in `PRAGMA user_version`.
const SCHEMA_VERSION: i64 = 5;

/// Brings any older database up to `SCHEMA_VERSION`. Version 1 (the first release) had no
/// `pages.hops` column; versions 1-2 lacked tables that `SCHEMA` creates if missing; version
/// 3 had no `events.records` column or `records` table; version 4 had no
/// `pages.content_at`, `pages.continuation` or `revisits`.
/// A database from a newer rayap is refused rather than changed.
fn migrate(tx: &Transaction) -> Result<()> {
    let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version > SCHEMA_VERSION {
        return Err(crate::Error::Config(format!(
            "database schema version {version} is newer than this rayap supports ({SCHEMA_VERSION}); upgrade rayap"
        )));
    }
    let has_pages: bool = tx.query_row(
        "SELECT count(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'pages'",
        [],
        |r| r.get(0),
    )?;
    if has_pages && version < 2 {
        let has_hops: bool = tx.query_row(
            "SELECT count(*) > 0 FROM pragma_table_info('pages') WHERE name = 'hops'",
            [],
            |r| r.get(0),
        )?;
        if !has_hops {
            tx.execute_batch("ALTER TABLE pages ADD COLUMN hops INTEGER NOT NULL DEFAULT 0")?;
        }
    }
    let has_events: bool = tx.query_row(
        "SELECT count(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'events'",
        [],
        |r| r.get(0),
    )?;
    if has_events && version < 4 {
        let has_records: bool = tx.query_row(
            "SELECT count(*) > 0 FROM pragma_table_info('events') WHERE name = 'records'",
            [],
            |r| r.get(0),
        )?;
        if !has_records {
            tx.execute_batch("ALTER TABLE events ADD COLUMN records INTEGER")?;
        }
    }
    if has_pages && version < 5 {
        for (column, definition) in [
            ("content_at", "INTEGER"),
            ("continuation", "INTEGER NOT NULL DEFAULT 0"),
            ("activity_hash", "TEXT"),
        ] {
            let present: bool = tx.query_row(
                "SELECT count(*) > 0 FROM pragma_table_info('pages') WHERE name = ?1",
                [column],
                |r| r.get(0),
            )?;
            if !present {
                tx.execute_batch(&format!(
                    "ALTER TABLE pages ADD COLUMN {column} {definition}"
                ))?;
            }
        }
        // Pages fetched before this version: their last content fetch is their last fetch,
        // and TikTok comment pages (the only continuation pages so far) are marked as such.
        tx.execute_batch(
            "UPDATE pages SET content_at = fetched_at WHERE content_at IS NULL AND content_hash IS NOT NULL;
             UPDATE pages SET continuation = 1 WHERE url LIKE '%/api/comment/list/?%';",
        )?;
    }
    tx.execute_batch(SCHEMA)?;
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

fn enqueue_in(conn: &Connection, run: i64, page: &NewPage) -> Result<bool> {
    let changed = conn
        .prepare_cached(
            "INSERT INTO pages (url, source, depth, hops, continuation, queued_run)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (url) DO UPDATE SET queued_run = excluded.queued_run, attempts = 0,
                 source = excluded.source, depth = excluded.depth, hops = excluded.hops,
                 continuation = excluded.continuation
             WHERE pages.queued_run < excluded.queued_run",
        )?
        .execute(params![
            page.url,
            page.source,
            page.depth,
            page.hops,
            page.continuation,
            run
        ])?;
    Ok(changed > 0)
}

fn complete_in(
    tx: &Transaction,
    run: i64,
    url: &str,
    kind: Option<EventKind>,
    o: Outcome,
) -> Result<(Event, bool)> {
    let (source, prev_hash, content_at, prev_activity): (
        String,
        Option<String>,
        Option<i64>,
        Option<String>,
    ) = tx
        .prepare_cached(
            "SELECT source, content_hash, content_at, activity_hash FROM pages WHERE url = ?1",
        )?
        .query_row([url], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
    // Only a comparison counts: a first activity reading is covered by the page being New.
    let activity_changed = match (&o.activity, &prev_activity) {
        (Some(now), Some(prev)) => now != prev,
        _ => false,
    };
    let kind = match (&o.hash, kind) {
        (Some(hash), _) => match &prev_hash {
            None => EventKind::New,
            Some(prev) if prev == hash => EventKind::Unchanged,
            Some(_) => EventKind::Changed,
        },
        (None, Some(kind)) => kind,
        (None, None) => EventKind::Error,
    };
    let now = unix_now();
    if o.hash.is_some() {
        tx.prepare_cached(
            "UPDATE pages SET done_run = ?1, status = ?2, etag = ?3, last_modified = ?4, content_hash = ?5,
                 title = ?6, fetched_at = ?7, content_at = ?7, activity_hash = ?9 WHERE url = ?8",
        )?
        .execute(params![
            run,
            o.status,
            o.etag,
            o.last_modified,
            o.hash,
            o.title,
            now,
            url,
            o.activity
        ])?;
        // What a revisit showed, for the change-rate estimate: only comparisons count, so
        // a first fetch records nothing.
        if let (Some(previous), Some(_)) = (content_at, &prev_hash) {
            let interval_days = (now - previous).max(0) as f64 / SECONDS_PER_DAY;
            tx.prepare_cached(
                "INSERT INTO revisits (url, at, interval_days, changed) VALUES (?1, ?2, ?3, ?4)",
            )?
            .execute(params![url, now, interval_days, kind == EventKind::Changed])?;
        }
    } else {
        // Keep the last good fingerprint so a page that comes back is compared with it.
        tx.prepare_cached(
            "UPDATE pages SET done_run = ?1, status = ?2, fetched_at = ?3 WHERE url = ?4",
        )?
        .execute(params![run, o.status, now, url])?;
    }
    let prev_hash = if o.hash.is_some() { prev_hash } else { None };
    let records = if o.records.is_empty() {
        None
    } else {
        let mut insert = tx.prepare_cached(
            "INSERT INTO records (run, at, source, url, kind, key, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT (kind, key) DO UPDATE SET run = excluded.run, at = excluded.at,
                 source = excluded.source, url = excluded.url, data = excluded.data",
        )?;
        for r in &o.records {
            let data = serde_json::to_string(&r.data).expect("serde_json::Value serializes");
            insert.execute(params![run, now, source, url, r.kind, r.key, data])?;
        }
        Some(o.records.len() as i64)
    };
    tx.prepare_cached(
        "INSERT INTO events (run, at, source, url, kind, status, title, hash, prev_hash, bytes, detail, records)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )?
    .execute(params![run, now, source, url, kind.as_str(), o.status, o.title, o.hash, prev_hash, o.bytes, o.detail, records])?;
    Ok((
        Event {
            id: tx.last_insert_rowid(),
            run,
            at: now,
            source,
            url: url.to_string(),
            kind,
            status: o.status,
            title: o.title,
            hash: o.hash,
            prev_hash,
            bytes: o.bytes,
            detail: o.detail,
            records,
        },
        activity_changed,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(kind: &str, key: &str, text: &str) -> NewRecord {
        NewRecord {
            kind: kind.into(),
            key: key.into(),
            data: serde_json::json!({ "text": text }),
        }
    }

    #[test]
    fn records_commit_with_the_event_and_are_replaced_by_key() {
        let mut s = Store::open_in_memory().unwrap();
        let (r1, _) = begin(&mut s);
        s.enqueue(r1, &page("https://a.org/")).unwrap();
        let outcome = Outcome {
            records: vec![record("post", "1", "hello"), record("comment", "9", "hi")],
            ..content("h1")
        };
        let e = done(&mut s, r1, "https://a.org/", outcome);
        assert_eq!(e.records, Some(2));
        assert_eq!(
            done(&mut s, r1, "https://a.org/", content("h1")).records,
            None
        );
        s.finish_run(r1).unwrap();

        let (r2, _) = begin(&mut s);
        let outcome = Outcome {
            records: vec![record("post", "1", "edited")],
            ..content("h2")
        };
        done(&mut s, r2, "https://a.org/", outcome);
        let all = s.records_after(0, None, 10).unwrap();
        assert_eq!(all.len(), 2, "same key replaces, not duplicates");
        let posts = s.records_after(0, Some("post"), 10).unwrap();
        assert_eq!(posts.len(), 1);
        assert_eq!(
            (posts[0].run, posts[0].data["text"].as_str()),
            (r2, Some("edited"))
        );
        assert!(
            s.records_after(posts[0].id, Some("post"), 10)
                .unwrap()
                .is_empty()
        );
    }

    fn page(url: &str) -> NewPage {
        NewPage {
            url: url.into(),
            source: "src".into(),
            depth: 0,
            hops: 0,
            continuation: false,
        }
    }

    fn done(s: &mut Store, run: i64, url: &str, outcome: Outcome) -> Event {
        s.complete(run, url, None, outcome, Vec::new(), Vec::new())
            .unwrap()
            .0
    }

    fn begin(s: &mut Store) -> (i64, bool) {
        let start = s.begin_run(&["src"], None).unwrap();
        (start.run, start.resumed)
    }

    fn plan(budget: usize) -> Plan {
        Plan {
            budget,
            max_interval_days: 30.0,
            prior: Prior {
                rate: 0.2,
                weight: 0.5,
            },
        }
    }

    /// Backdates a page's last fetch so a plan sees time as having passed.
    fn age(s: &Store, url: &str, days: f64) {
        let at = unix_now() - (days * SECONDS_PER_DAY) as i64;
        s.conn
            .execute(
                "UPDATE pages SET fetched_at = ?1, content_at = ?1 WHERE url = ?2",
                params![at, url],
            )
            .unwrap();
    }

    fn observe(s: &Store, url: &str, interval_days: f64, changed: bool) {
        s.conn
            .execute(
                "INSERT INTO revisits (url, at, interval_days, changed) VALUES (?1, ?2, ?3, ?4)",
                params![url, unix_now(), interval_days, changed],
            )
            .unwrap();
    }

    #[test]
    fn revisits_record_interval_and_change_only_when_comparable() {
        let mut s = Store::open_in_memory().unwrap();
        let (r1, _) = begin(&mut s);
        s.enqueue(r1, &page("https://a.org/")).unwrap();
        done(&mut s, r1, "https://a.org/", content("h1"));
        let count = |s: &Store| -> i64 {
            s.conn
                .query_row("SELECT count(*) FROM revisits", [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(count(&s), 0, "a first fetch is not a revisit");
        s.finish_run(r1).unwrap();
        age(&s, "https://a.org/", 2.0);

        let (r2, _) = begin(&mut s);
        let gone = Outcome {
            status: Some(500),
            ..Outcome::default()
        };
        s.complete(
            r2,
            "https://a.org/",
            Some(EventKind::Error),
            gone,
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        assert_eq!(count(&s), 0, "an error is not a comparison");
        s.finish_run(r2).unwrap();

        let (r3, _) = begin(&mut s);
        done(&mut s, r3, "https://a.org/", content("h2"));
        let (interval, changed): (f64, bool) = s
            .conn
            .query_row("SELECT interval_days, changed FROM revisits", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert!(
            (interval - 2.0).abs() < 0.01,
            "measured from the last content fetch, not the error: {interval}"
        );
        assert!(changed);
    }

    #[test]
    fn planned_run_takes_unfetched_pages_then_the_most_overdue() {
        let mut s = Store::open_in_memory().unwrap();
        let (r1, _) = begin(&mut s);
        for u in [
            "https://a.org/busy",
            "https://a.org/quiet",
            "https://a.org/fresh",
        ] {
            s.enqueue(r1, &page(u)).unwrap();
            done(&mut s, r1, u, content(u));
        }
        s.enqueue(r1, &page("https://a.org/never")).unwrap();
        s.finish_run(r1).unwrap();
        // busy changed on 4 of 10 daily visits; quiet on none; fresh was fetched just now.
        for i in 0..10 {
            observe(&s, "https://a.org/busy", 1.0, i % 5 < 2);
            observe(&s, "https://a.org/quiet", 1.0, false);
        }
        age(&s, "https://a.org/busy", 3.0);
        age(&s, "https://a.org/quiet", 3.0);

        let start = s.begin_run(&["src"], Some(&plan(1))).unwrap();
        assert!(!start.resumed);
        let pending: Vec<String> = s
            .pending(start.run)
            .unwrap()
            .into_iter()
            .map(|p| p.url)
            .collect();
        assert_eq!(
            pending,
            ["https://a.org/busy", "https://a.org/never"],
            "the never-fetched page is free; the one budget slot goes to the overdue page that changes"
        );
        assert_eq!(
            start.deferred, 2,
            "quiet (rarely changes) and fresh (just fetched) wait"
        );

        let rates = s.rates("src", plan(1).prior).unwrap();
        let rate = |u: &str| rates.iter().find(|(x, _)| x == u).unwrap().1;
        assert!(
            rate("https://a.org/busy") > 0.3,
            "{}",
            rate("https://a.org/busy")
        );
        assert!(
            rate("https://a.org/quiet") < 0.05,
            "{}",
            rate("https://a.org/quiet")
        );
        assert_eq!(rates.len(), 4);
    }

    #[test]
    fn continuation_pages_follow_only_new_or_changed_and_are_never_planned() {
        let mut s = Store::open_in_memory().unwrap();
        let next = NewPage {
            continuation: true,
            ..page("https://a.org/?cursor=20")
        };
        let (r1, _) = begin(&mut s);
        s.enqueue(r1, &page("https://a.org/")).unwrap();
        let (_, added) = s
            .complete(
                r1,
                "https://a.org/",
                None,
                content("h1"),
                Vec::new(),
                vec![next.clone()],
            )
            .unwrap();
        assert_eq!(
            added,
            [next.clone()].as_slice(),
            "new: the continuation is queued"
        );
        done(&mut s, r1, "https://a.org/?cursor=20", content("c1"));
        s.finish_run(r1).unwrap();

        age(&s, "https://a.org/", 40.0);
        age(&s, "https://a.org/?cursor=20", 40.0);
        let start = s.begin_run(&["src"], Some(&plan(10))).unwrap();
        let pending: Vec<String> = s
            .pending(start.run)
            .unwrap()
            .into_iter()
            .map(|p| p.url)
            .collect();
        assert_eq!(
            pending,
            ["https://a.org/"],
            "the continuation is not planned on its own"
        );
        let (_, added) = s
            .complete(
                start.run,
                "https://a.org/",
                None,
                content("h1"),
                Vec::new(),
                vec![next.clone()],
            )
            .unwrap();
        assert!(
            added.is_empty(),
            "unchanged: the continuation is not fetched"
        );
        s.finish_run(start.run).unwrap();

        age(&s, "https://a.org/", 40.0);
        let start = s.begin_run(&["src"], Some(&plan(10))).unwrap();
        let (_, added) = s
            .complete(
                start.run,
                "https://a.org/",
                None,
                content("h2"),
                Vec::new(),
                vec![next.clone()],
            )
            .unwrap();
        assert_eq!(added, [next], "changed: the continuation is fetched again");
    }

    #[test]
    fn activity_change_alone_refetches_the_continuation() {
        let mut s = Store::open_in_memory().unwrap();
        let next = NewPage {
            continuation: true,
            ..page("https://a.org/?cursor=20")
        };
        let with_activity = |a: &str| Outcome {
            activity: Some(a.into()),
            ..content("same")
        };
        let (r1, _) = begin(&mut s);
        s.enqueue(r1, &page("https://a.org/")).unwrap();
        s.complete(
            r1,
            "https://a.org/",
            None,
            with_activity("c=10"),
            Vec::new(),
            vec![next.clone()],
        )
        .unwrap();
        done(&mut s, r1, "https://a.org/?cursor=20", content("c1"));
        s.finish_run(r1).unwrap();

        // Planned runs do not queue continuation pages on their own, so what gets added
        // here is decided by the activity comparison alone.
        age(&s, "https://a.org/", 40.0);
        let r2 = s.begin_run(&["src"], Some(&plan(10))).unwrap().run;
        let (e, added) = s
            .complete(
                r2,
                "https://a.org/",
                None,
                with_activity("c=10"),
                Vec::new(),
                vec![next.clone()],
            )
            .unwrap();
        assert_eq!(
            (e.kind, added.len()),
            (EventKind::Unchanged, 0),
            "same content, same activity"
        );
        s.finish_run(r2).unwrap();

        age(&s, "https://a.org/", 40.0);
        let r3 = s.begin_run(&["src"], Some(&plan(10))).unwrap().run;
        let (e, added) = s
            .complete(
                r3,
                "https://a.org/",
                None,
                with_activity("c=11"),
                Vec::new(),
                vec![next.clone()],
            )
            .unwrap();
        assert_eq!(
            e.kind,
            EventKind::Unchanged,
            "the page itself did not change"
        );
        assert_eq!(
            added,
            [next],
            "but its activity did, so the continuation is fetched"
        );
    }

    #[test]
    fn seeds_are_not_forced_into_a_planned_run() {
        let mut s = Store::open_in_memory().unwrap();
        let (r1, _) = begin(&mut s);
        s.enqueue(r1, &page("https://a.org/")).unwrap();
        done(&mut s, r1, "https://a.org/", content("h"));
        s.finish_run(r1).unwrap();
        let start = s.begin_run(&["src"], Some(&plan(0))).unwrap();
        assert!(
            !s.enqueue_if_unknown(start.run, &page("https://a.org/"))
                .unwrap()
        );
        assert!(
            s.enqueue_if_unknown(start.run, &page("https://a.org/new"))
                .unwrap()
        );
        let pending: Vec<String> = s
            .pending(start.run)
            .unwrap()
            .into_iter()
            .map(|p| p.url)
            .collect();
        assert_eq!(pending, ["https://a.org/new"]);
    }

    #[test]
    fn version_3_database_gains_the_records_column() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v3.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE runs (id INTEGER PRIMARY KEY, started_at INTEGER NOT NULL, finished_at INTEGER);
                 CREATE TABLE pages (url TEXT PRIMARY KEY, source TEXT NOT NULL, depth INTEGER NOT NULL,
                     hops INTEGER NOT NULL DEFAULT 0, queued_run INTEGER NOT NULL, done_run INTEGER,
                     attempts INTEGER NOT NULL DEFAULT 0, status INTEGER, etag TEXT, last_modified TEXT,
                     content_hash TEXT, title TEXT, fetched_at INTEGER);
                 CREATE TABLE events (id INTEGER PRIMARY KEY, run INTEGER NOT NULL, at INTEGER NOT NULL,
                     source TEXT NOT NULL, url TEXT NOT NULL, kind TEXT NOT NULL, status INTEGER,
                     title TEXT, hash TEXT, prev_hash TEXT, bytes INTEGER, detail TEXT);
                 INSERT INTO events (run, at, source, url, kind) VALUES (1, 0, 'src', 'https://x.org/', 'new');
                 PRAGMA user_version = 3;",
            )
            .unwrap();
        }
        let s = Store::open(&path).unwrap();
        let events = s.events_after(0, 10).unwrap();
        assert_eq!((events.len(), events[0].records), (1, None));
        assert!(s.records_after(0, None, 10).unwrap().is_empty());
    }

    fn content(hash: &str) -> Outcome {
        Outcome {
            status: Some(200),
            hash: Some(hash.into()),
            ..Outcome::default()
        }
    }

    #[test]
    fn new_then_unchanged_then_changed() {
        let mut s = Store::open_in_memory().unwrap();
        let (r1, resumed) = begin(&mut s);
        assert!(!resumed);
        assert!(s.enqueue(r1, &page("https://a.org/")).unwrap());
        assert!(
            !s.enqueue(r1, &page("https://a.org/")).unwrap(),
            "already in this run"
        );
        assert_eq!(
            done(&mut s, r1, "https://a.org/", content("h1")).kind,
            EventKind::New
        );
        s.finish_run(r1).unwrap();

        let (r2, _) = begin(&mut s);
        assert_eq!(
            s.pending(r2).unwrap().len(),
            1,
            "known pages are re-queued in a new run"
        );
        assert_eq!(
            done(&mut s, r2, "https://a.org/", content("h1")).kind,
            EventKind::Unchanged
        );
        s.finish_run(r2).unwrap();

        let (r3, _) = begin(&mut s);
        let e = done(&mut s, r3, "https://a.org/", content("h2"));
        assert_eq!(
            (e.kind, e.prev_hash.as_deref()),
            (EventKind::Changed, Some("h1"))
        );
    }

    #[test]
    fn errors_keep_the_last_good_fingerprint() {
        let mut s = Store::open_in_memory().unwrap();
        let (r1, _) = begin(&mut s);
        s.enqueue(r1, &page("https://a.org/")).unwrap();
        done(&mut s, r1, "https://a.org/", content("h1"));
        s.finish_run(r1).unwrap();

        let (r2, _) = begin(&mut s);
        let gone = Outcome {
            status: Some(404),
            ..Outcome::default()
        };
        assert_eq!(
            s.complete(
                r2,
                "https://a.org/",
                Some(EventKind::Gone),
                gone,
                Vec::new(),
                Vec::new()
            )
            .unwrap()
            .0
            .kind,
            EventKind::Gone
        );
        s.finish_run(r2).unwrap();

        let (r3, _) = begin(&mut s);
        assert_eq!(
            done(&mut s, r3, "https://a.org/", content("h1")).kind,
            EventKind::Unchanged
        );
    }

    #[test]
    fn unfinished_run_is_resumed_with_only_pending_pages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        {
            let mut s = Store::open(&path).unwrap();
            let (r1, _) = begin(&mut s);
            for u in ["https://a.org/1", "https://a.org/2"] {
                s.enqueue(r1, &page(u)).unwrap();
            }
            done(&mut s, r1, "https://a.org/1", content("h"));
            s.record_attempt("https://a.org/2", 2).unwrap();
            // process stops here without finish_run
        }
        let mut s = Store::open(&path).unwrap();
        let (run, resumed) = begin(&mut s);
        assert!(resumed);
        assert_eq!(
            s.pending(run).unwrap(),
            [PendingPage {
                url: "https://a.org/2".into(),
                source: "src".into(),
                depth: 0,
                hops: 0,
                attempts: 2
            }]
        );
    }

    #[test]
    fn removed_sources_are_not_requeued() {
        let mut s = Store::open_in_memory().unwrap();
        let (r1, _) = {
            let st = s.begin_run(&["old"], None).unwrap();
            (st.run, st.resumed)
        };
        s.enqueue(
            r1,
            &NewPage {
                source: "old".into(),
                ..page("https://a.org/")
            },
        )
        .unwrap();
        done(&mut s, r1, "https://a.org/", content("h"));
        s.finish_run(r1).unwrap();
        let (r2, _) = {
            let st = s.begin_run(&["new"], None).unwrap();
            (st.run, st.resumed)
        };
        assert!(s.pending(r2).unwrap().is_empty());
    }

    #[test]
    fn version_1_database_is_upgraded_and_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v1.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE runs (id INTEGER PRIMARY KEY, started_at INTEGER NOT NULL, finished_at INTEGER);
                 CREATE TABLE pages (url TEXT PRIMARY KEY, source TEXT NOT NULL, depth INTEGER NOT NULL,
                     queued_run INTEGER NOT NULL, done_run INTEGER, attempts INTEGER NOT NULL DEFAULT 0,
                     status INTEGER, etag TEXT, last_modified TEXT, content_hash TEXT, title TEXT,
                     fetched_at INTEGER);
                 CREATE TABLE events (id INTEGER PRIMARY KEY, run INTEGER NOT NULL, at INTEGER NOT NULL,
                     source TEXT NOT NULL, url TEXT NOT NULL, kind TEXT NOT NULL, status INTEGER,
                     title TEXT, hash TEXT, prev_hash TEXT, bytes INTEGER, detail TEXT);
                 INSERT INTO runs (id, started_at) VALUES (1, 0);
                 INSERT INTO pages (url, source, depth, queued_run) VALUES ('https://a.org/', 'src', 0, 1);
                 INSERT INTO events (run, at, source, url, kind) VALUES (1, 0, 'src', 'https://x.org/', 'new');",
            )
            .unwrap();
        }
        let mut s = Store::open(&path).unwrap();
        let (run, resumed) = begin(&mut s);
        assert!(resumed);
        assert_eq!(s.pending(run).unwrap()[0].hops, 0);
        assert_eq!(s.events_after(0, 10).unwrap().len(), 1, "old events kept");
        drop(s);
        Store::open(&path).unwrap(); // opening again is a no-op
    }

    #[test]
    fn newer_schema_is_refused_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch("CREATE TABLE pages (url TEXT); PRAGMA user_version = 99;")
                .unwrap();
        }
        assert!(matches!(Store::open(&path), Err(crate::Error::Config(m)) if m.contains("newer")));
        let conn = Connection::open(&path).unwrap();
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 99);
    }

    #[test]
    fn robots_failures_are_scoped_to_the_run() {
        let s = Store::open_in_memory().unwrap();
        s.record_robots_failure(1, "https://a.org", 2).unwrap();
        assert_eq!(
            s.robots_failures(1).unwrap(),
            [("https://a.org".to_string(), 2)]
        );
        assert!(s.robots_failures(2).unwrap().is_empty());
        s.clear_robots_failures(1, "https://a.org").unwrap();
        assert!(s.robots_failures(1).unwrap().is_empty());
    }

    #[test]
    fn discovered_pages_commit_with_the_outcome() {
        let mut s = Store::open_in_memory().unwrap();
        let (r, _) = begin(&mut s);
        s.enqueue(r, &page("https://a.org/")).unwrap();
        let links = vec![
            page("https://a.org/1"),
            page("https://a.org/"),
            page("https://a.org/1"),
        ];
        let (_, added) = s
            .complete(r, "https://a.org/", None, content("h"), links, Vec::new())
            .unwrap();
        assert_eq!(
            added,
            [page("https://a.org/1")],
            "only pages new to the run are returned"
        );
        assert_eq!(s.pending(r).unwrap().len(), 1);
    }

    #[test]
    fn host_holds_persist_and_keep_the_later_time() {
        let s = Store::open_in_memory().unwrap();
        let far = unix_now_ms() + 3_600_000;
        s.hold_host("a.org", far).unwrap();
        s.hold_host("a.org", far - 1000).unwrap();
        s.hold_host("old.org", unix_now_ms() - 1).unwrap();
        assert_eq!(s.active_holds().unwrap(), [("a.org".to_string(), far)]);
    }

    #[test]
    fn events_after_pages_through_ids() {
        let mut s = Store::open_in_memory().unwrap();
        let (r, _) = begin(&mut s);
        for u in ["https://a.org/1", "https://a.org/2", "https://a.org/3"] {
            s.enqueue(r, &page(u)).unwrap();
            done(&mut s, r, u, content(u));
        }
        let first = s.events_after(0, 2).unwrap();
        assert_eq!(first.len(), 2);
        let rest = s.events_after(first[1].id, 10).unwrap();
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].url, "https://a.org/3");
    }
}

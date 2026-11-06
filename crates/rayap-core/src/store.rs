//! Durable crawl state in SQLite.
//!
//! A *run* visits every known page once. A page is pending in run R until its outcome is
//! committed with `done_run = R`. The outcome and its event are written in one transaction,
//! so after a crash the page is simply fetched again: delivery is at-least-once, and an
//! unfinished run is resumed instead of starting over.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Serialize;

use crate::Result;

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
    fetched_at    INTEGER
);
CREATE INDEX IF NOT EXISTS pages_pending ON pages (queued_run, done_run);
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
    detail    TEXT
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
    /// re-queues every known page whose source is still configured.
    /// The bool is true when an unfinished run was resumed.
    pub fn begin_run(&mut self, sources: &[&str]) -> Result<(i64, bool)> {
        let open: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM runs WHERE finished_at IS NULL ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(run) = open {
            return Ok((run, true));
        }
        let tx = self.conn.transaction()?;
        tx.execute("INSERT INTO runs (started_at) VALUES (?1)", [unix_now()])?;
        let run = tx.last_insert_rowid();
        {
            let mut requeue =
                tx.prepare("UPDATE pages SET queued_run = ?1, attempts = 0 WHERE source = ?2")?;
            for source in sources {
                requeue.execute(params![run, source])?;
            }
        }
        tx.commit()?;
        Ok((run, false))
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
        self.conn.execute(
            "UPDATE pages SET attempts = ?1 WHERE url = ?2",
            params![attempts, url],
        )?;
        Ok(())
    }

    /// Commits the page outcome, its event and the pages it led to, in one transaction.
    /// Returns the event and the discovered pages that are new to this run (to be put in the
    /// frontier). For content outcomes (`hash` is set) the kind is decided here from the
    /// stored hash: `New`, `Changed` or `Unchanged`. Other kinds are taken as given.
    pub fn complete(
        &mut self,
        run: i64,
        url: &str,
        kind: Option<EventKind>,
        outcome: Outcome,
        discovered: Vec<NewPage>,
    ) -> Result<(Event, Vec<NewPage>)> {
        let tx = self.conn.transaction()?;
        let event = complete_in(&tx, run, url, kind, outcome)?;
        let mut added = Vec::new();
        for page in discovered {
            if enqueue_in(&tx, run, &page)? {
                added.push(page);
            }
        }
        tx.commit()?;
        Ok((event, added))
    }

    /// Persists that `host` must not be contacted before `until_ms` (Unix milliseconds).
    /// An existing later hold is kept.
    pub fn hold_host(&self, host: &str, until_ms: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO host_holds (host, until_ms) VALUES (?1, ?2)
             ON CONFLICT (host) DO UPDATE SET until_ms = max(until_ms, excluded.until_ms)",
            params![host, until_ms],
        )?;
        Ok(())
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

    /// Events with `id > after`, oldest first.
    pub fn events_after(&self, after: i64, limit: usize) -> Result<Vec<Event>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, run, at, source, url, kind, status, title, hash, prev_hash, bytes, detail
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
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }
}

/// Schema version stored in `PRAGMA user_version`.
const SCHEMA_VERSION: i64 = 2;

/// Brings any older database up to `SCHEMA_VERSION`. Version 1 (the first release) had no
/// `pages.hops` column and no `host_holds` table.
fn migrate(tx: &Transaction) -> Result<()> {
    let version: i64 = tx.query_row("PRAGMA user_version", [], |r| r.get(0))?;
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
    tx.execute_batch(SCHEMA)?;
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    Ok(())
}

fn enqueue_in(conn: &Connection, run: i64, page: &NewPage) -> Result<bool> {
    let changed = conn.execute(
        "INSERT INTO pages (url, source, depth, hops, queued_run) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (url) DO UPDATE SET queued_run = excluded.queued_run, attempts = 0,
             source = excluded.source, depth = excluded.depth, hops = excluded.hops
         WHERE pages.queued_run < excluded.queued_run",
        params![page.url, page.source, page.depth, page.hops, run],
    )?;
    Ok(changed > 0)
}

fn complete_in(
    tx: &Transaction,
    run: i64,
    url: &str,
    kind: Option<EventKind>,
    o: Outcome,
) -> Result<Event> {
    let (source, prev_hash): (String, Option<String>) = tx.query_row(
        "SELECT source, content_hash FROM pages WHERE url = ?1",
        [url],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
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
        tx.execute(
            "UPDATE pages SET done_run = ?1, status = ?2, etag = ?3, last_modified = ?4, content_hash = ?5,
                 title = ?6, fetched_at = ?7 WHERE url = ?8",
            params![run, o.status, o.etag, o.last_modified, o.hash, o.title, now, url],
        )?;
    } else {
        // Keep the last good fingerprint so a page that comes back is compared with it.
        tx.execute(
            "UPDATE pages SET done_run = ?1, status = ?2, fetched_at = ?3 WHERE url = ?4",
            params![run, o.status, now, url],
        )?;
    }
    let prev_hash = if o.hash.is_some() { prev_hash } else { None };
    tx.execute(
        "INSERT INTO events (run, at, source, url, kind, status, title, hash, prev_hash, bytes, detail)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![run, now, source, url, kind.as_str(), o.status, o.title, o.hash, prev_hash, o.bytes, o.detail],
    )?;
    Ok(Event {
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(url: &str) -> NewPage {
        NewPage {
            url: url.into(),
            source: "src".into(),
            depth: 0,
            hops: 0,
        }
    }

    fn done(s: &mut Store, run: i64, url: &str, outcome: Outcome) -> Event {
        s.complete(run, url, None, outcome, Vec::new()).unwrap().0
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
        let (r1, resumed) = s.begin_run(&["src"]).unwrap();
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

        let (r2, _) = s.begin_run(&["src"]).unwrap();
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

        let (r3, _) = s.begin_run(&["src"]).unwrap();
        let e = done(&mut s, r3, "https://a.org/", content("h2"));
        assert_eq!(
            (e.kind, e.prev_hash.as_deref()),
            (EventKind::Changed, Some("h1"))
        );
    }

    #[test]
    fn errors_keep_the_last_good_fingerprint() {
        let mut s = Store::open_in_memory().unwrap();
        let (r1, _) = s.begin_run(&["src"]).unwrap();
        s.enqueue(r1, &page("https://a.org/")).unwrap();
        done(&mut s, r1, "https://a.org/", content("h1"));
        s.finish_run(r1).unwrap();

        let (r2, _) = s.begin_run(&["src"]).unwrap();
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
                Vec::new()
            )
            .unwrap()
            .0
            .kind,
            EventKind::Gone
        );
        s.finish_run(r2).unwrap();

        let (r3, _) = s.begin_run(&["src"]).unwrap();
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
            let (r1, _) = s.begin_run(&["src"]).unwrap();
            for u in ["https://a.org/1", "https://a.org/2"] {
                s.enqueue(r1, &page(u)).unwrap();
            }
            done(&mut s, r1, "https://a.org/1", content("h"));
            s.record_attempt("https://a.org/2", 2).unwrap();
            // process stops here without finish_run
        }
        let mut s = Store::open(&path).unwrap();
        let (run, resumed) = s.begin_run(&["src"]).unwrap();
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
        let (r1, _) = s.begin_run(&["old"]).unwrap();
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
        let (r2, _) = s.begin_run(&["new"]).unwrap();
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
        let (run, resumed) = s.begin_run(&["src"]).unwrap();
        assert!(resumed);
        assert_eq!(s.pending(run).unwrap()[0].hops, 0);
        assert_eq!(s.events_after(0, 10).unwrap().len(), 1, "old events kept");
        drop(s);
        Store::open(&path).unwrap(); // opening again is a no-op
    }

    #[test]
    fn discovered_pages_commit_with_the_outcome() {
        let mut s = Store::open_in_memory().unwrap();
        let (r, _) = s.begin_run(&["src"]).unwrap();
        s.enqueue(r, &page("https://a.org/")).unwrap();
        let links = vec![
            page("https://a.org/1"),
            page("https://a.org/"),
            page("https://a.org/1"),
        ];
        let (_, added) = s
            .complete(r, "https://a.org/", None, content("h"), links)
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
        let (r, _) = s.begin_run(&["src"]).unwrap();
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

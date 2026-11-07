//! The one task that writes the store.
//!
//! SQLite allows one writer per database. With several coordinators (shards) crawling at
//! once, each sends its writes here as commands over a channel; this task applies them in
//! arrival order, in batches, one transaction per batch, and answers the commands that need
//! a result (`complete` returns the event and the pages new to the run). Everything a shard
//! sends between two `Flush` markers is committed together, so a shard's "deliver events
//! after commit" rule holds exactly as it did with a single coordinator.
//!
//! Ordering: commands from one shard are applied in the order sent (one channel, FIFO), so
//! "hold first, then record the attempt" stays true per host. Commands from different shards
//! interleave, which is safe because shards never touch the same host.

use tokio::sync::{mpsc, oneshot};

use crate::store::{Event, EventKind, NewPage, Outcome, Store};
use crate::{Error, Result};

/// Outcomes applied per transaction at most; see `MAX_BATCH` in the crawler.
const MAX_BATCH: usize = 512;

pub enum Command {
    Complete(Box<Complete>),
    HoldHost {
        host: String,
        until_ms: i64,
    },
    RecordAttempt {
        url: String,
        attempts: u32,
    },
    RecordRobotsFailure {
        run: i64,
        origin: String,
        failures: u32,
    },
    ClearRobotsFailures {
        run: i64,
        origin: String,
    },
    /// Commit what has been applied so far and tell the sender it is durable.
    Flush {
        reply: oneshot::Sender<Result<()>>,
    },
}

pub struct Complete {
    run: i64,
    url: String,
    kind: Option<EventKind>,
    outcome: Outcome,
    discovered: Vec<NewPage>,
    continuation: Vec<NewPage>,
    reply: oneshot::Sender<Result<(Event, Vec<NewPage>)>>,
}

/// A shard's handle on the writer.
#[derive(Clone)]
pub struct Writer {
    tx: mpsc::UnboundedSender<Command>,
}

impl Writer {
    fn send(&self, command: Command) -> Result<()> {
        self.tx
            .send(command)
            .map_err(|_| Error::Task("store writer stopped".into()))
    }

    pub async fn complete(
        &self,
        run: i64,
        url: &str,
        kind: Option<EventKind>,
        outcome: Outcome,
        discovered: Vec<NewPage>,
        continuation: Vec<NewPage>,
    ) -> Result<(Event, Vec<NewPage>)> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::Complete(Box::new(Complete {
            run,
            url: url.to_string(),
            kind,
            outcome,
            discovered,
            continuation,
            reply,
        })))?;
        answer
            .await
            .map_err(|_| Error::Task("store writer dropped a reply".into()))?
    }

    pub fn hold_host(&self, host: &str, until_ms: i64) -> Result<()> {
        self.send(Command::HoldHost {
            host: host.to_string(),
            until_ms,
        })
    }

    pub fn record_attempt(&self, url: &str, attempts: u32) -> Result<()> {
        self.send(Command::RecordAttempt {
            url: url.to_string(),
            attempts,
        })
    }

    pub fn record_robots_failure(&self, run: i64, origin: &str, failures: u32) -> Result<()> {
        self.send(Command::RecordRobotsFailure {
            run,
            origin: origin.to_string(),
            failures,
        })
    }

    pub fn clear_robots_failures(&self, run: i64, origin: &str) -> Result<()> {
        self.send(Command::ClearRobotsFailures {
            run,
            origin: origin.to_string(),
        })
    }

    /// Resolves once everything this handle sent before is committed.
    pub async fn flush(&self) -> Result<()> {
        let (reply, answer) = oneshot::channel();
        self.send(Command::Flush { reply })?;
        answer
            .await
            .map_err(|_| Error::Task("store writer dropped a reply".into()))?
    }
}

/// Runs the writer over `store` until every `Writer` handle is dropped. Returns the store
/// so the caller can finish the run on it.
pub fn spawn(store: Store) -> (Writer, tokio::task::JoinHandle<Result<Store>>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = tokio::task::spawn_blocking(move || run(store, rx));
    (Writer { tx }, handle)
}

fn run(mut store: Store, mut rx: mpsc::UnboundedReceiver<Command>) -> Result<Store> {
    // Commands arrive in bursts (a shard drains its finished responses, then flushes). Take
    // everything available, apply it inside one transaction, commit, then answer flushes.
    let mut pending_flushes: Vec<oneshot::Sender<Result<()>>> = Vec::new();
    while let Some(first) = rx.blocking_recv() {
        store.begin_batch()?;
        let mut applied = 0;
        let mut next = Some(first);
        while let Some(command) = next.take() {
            if let Command::Flush { reply } = command {
                pending_flushes.push(reply);
            } else {
                apply(&mut store, command)?;
                applied += 1;
            }
            if applied >= MAX_BATCH {
                break;
            }
            next = rx.try_recv().ok();
        }
        let committed = store.end_batch();
        for reply in pending_flushes.drain(..) {
            let _ = reply.send(
                committed
                    .as_ref()
                    .map(|_| ())
                    .map_err(|e| Error::Task(e.to_string())),
            );
        }
        committed?;
    }
    Ok(store)
}

fn apply(store: &mut Store, command: Command) -> Result<()> {
    match command {
        Command::Complete(c) => {
            let Complete {
                run,
                url,
                kind,
                outcome,
                discovered,
                continuation,
                reply,
            } = *c;
            let result = store.complete(run, &url, kind, outcome, discovered, continuation);
            let failed = result.is_err();
            let _ = reply.send(result);
            if failed {
                return Err(Error::Task("store write failed".into()));
            }
        }
        Command::HoldHost { host, until_ms } => store.hold_host(&host, until_ms)?,
        Command::RecordAttempt { url, attempts } => store.record_attempt(&url, attempts)?,
        Command::RecordRobotsFailure {
            run,
            origin,
            failures,
        } => store.record_robots_failure(run, &origin, failures)?,
        Command::ClearRobotsFailures { run, origin } => {
            store.clear_robots_failures(run, &origin)?
        }
        Command::Flush { .. } => unreachable!("flushes are handled by the loop"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::unix_now_ms;

    fn page(url: &str) -> NewPage {
        NewPage {
            url: url.into(),
            source: "src".into(),
            depth: 0,
            hops: 0,
            continuation: false,
        }
    }

    #[tokio::test]
    async fn writes_from_two_handles_land_and_flush_means_committed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.db");
        let mut store = Store::open(&path).unwrap();
        let run = store.begin_run(&["src"], None).unwrap().run;
        for u in ["https://a.org/", "https://b.org/"] {
            store.enqueue(run, &page(u)).unwrap();
        }
        let (writer, task) = spawn(store);
        let (a, b) = (writer.clone(), writer.clone());
        drop(writer);

        let ta = tokio::spawn(async move {
            a.hold_host("a.org", unix_now_ms() + 60_000).unwrap();
            let (event, added) = a
                .complete(
                    run,
                    "https://a.org/",
                    None,
                    Outcome {
                        status: Some(200),
                        hash: Some("v2:a".into()),
                        ..Outcome::default()
                    },
                    vec![page("https://a.org/next")],
                    Vec::new(),
                )
                .await
                .unwrap();
            a.flush().await.unwrap();
            (event.kind, added.len())
        });
        let tb = tokio::spawn(async move {
            b.record_attempt("https://b.org/", 1).unwrap();
            b.flush().await.unwrap();
        });
        let (kind, added) = ta.await.unwrap();
        tb.await.unwrap();
        assert_eq!((kind, added), (EventKind::New, 1));

        // Both handles are gone: the writer stops and hands the store back, committed.
        let store = task.await.unwrap().unwrap();
        let pending = store.pending(run).unwrap();
        let urls: Vec<&str> = pending.iter().map(|p| p.url.as_str()).collect();
        assert_eq!(urls, ["https://b.org/", "https://a.org/next"]);
        assert_eq!(pending[0].attempts, 1);
        assert_eq!(store.events_after(0, 10).unwrap().len(), 1);
        assert_eq!(store.active_holds().unwrap().len(), 1);

        // And it is really on disk, not only in the connection.
        drop(store);
        let again = Store::open(&path).unwrap();
        assert_eq!(again.events_after(0, 10).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_failed_write_is_reported_to_the_sender() {
        let store = Store::open_in_memory().unwrap();
        let (writer, task) = spawn(store);
        // Completing a page that was never enqueued fails inside the store.
        let err = writer
            .complete(
                1,
                "https://nope/",
                None,
                Outcome::default(),
                Vec::new(),
                Vec::new(),
            )
            .await;
        assert!(err.is_err());
        drop(writer);
        assert!(
            task.await.unwrap().is_err(),
            "the writer stops on a store error"
        );
    }
}

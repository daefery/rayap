//! The crawl loop.
//!
//! One coordinator owns the frontier, the robots.txt cache and the store; fetches run as
//! tasks and report back over a channel. Because only the coordinator touches state, the
//! scheduling rules (one request in flight per host, delay between requests, robots.txt
//! before pages) hold without locks.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::{Duration, Instant, sleep_until};
use url::Url;

use crate::fetch::{self, Clients, PageResult};
use crate::frontier::{Frontier, Next, Task};
use crate::robots::{self, Rules};
use crate::store::{Event, EventKind, Outcome, Store};
use crate::{Config, Error, Result, html, urlnorm};

/// Wait after an unreachable robots.txt before asking again (the host is blocked meanwhile).
const ROBOTS_RETRY: Duration = Duration::from_secs(5 * 60);
/// Delay used for 429/503 without a usable Retry-After.
const SLOW_DOWN_DEFAULT: Duration = Duration::from_secs(60);
/// Longest Retry-After rayap will wait within a run; beyond that the page is reported as an error.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(60 * 60);

#[derive(Debug, Clone, Default)]
pub struct Options {
    /// Stop after this many page fetches (robots.txt requests not counted). Useful for tests
    /// and trial runs; an interrupted run resumes next time.
    pub max_fetches: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    pub run: i64,
    pub resumed: bool,
    pub requests: u64,
    pub events: u64,
    pub changed: u64,
    pub bytes: u64,
    /// True when every queued page got an outcome and the run was closed.
    pub finished: bool,
}

enum RobotsState {
    Fetching,
    Known { rules: Rules, fetched: Instant },
    Unreachable { until: Instant, reason: String },
}

enum Job {
    Robots { host: String, origin: String },
    Page { host: String, task: Task },
}

enum Step {
    Start(Job),
    /// Stop starting work for now; the next host becomes eligible at this time, if any.
    Wait(Option<Instant>),
    /// A task was settled without a request (blocked by robots.txt); ask again.
    Again,
}

enum Done {
    Robots {
        host: String,
        origin: String,
        fetched: robots::Fetched,
    },
    Page {
        host: String,
        task: Task,
        url: Url,
        result: PageResult,
    },
}

struct Crawler<'a, F: FnMut(&Event)> {
    config: &'a Config,
    store: &'a mut Store,
    run: i64,
    frontier: Frontier,
    robots: HashMap<String, RobotsState>,
    /// Tasks parked until their origin's robots.txt is known.
    waiting_for_robots: HashMap<String, Vec<(String, Task)>>,
    in_flight: usize,
    stats: Stats,
    on_event: F,
}

/// Crawls every configured source once, resuming an unfinished run if there is one.
/// `on_event` is called after each event is durably stored.
pub async fn crawl<F: FnMut(&Event)>(
    config: &Config,
    store: &mut Store,
    options: Options,
    on_event: F,
) -> Result<Stats> {
    let clients = Arc::new(Clients::new(config)?);
    let sources: Vec<&str> = config.sources.iter().map(|s| s.name.as_str()).collect();
    let (run, resumed) = store.begin_run(&sources)?;
    let now = Instant::now();
    let mut crawler = Crawler {
        config,
        store,
        run,
        frontier: Frontier::new(config.per_host_delay()),
        robots: HashMap::new(),
        waiting_for_robots: HashMap::new(),
        in_flight: 0,
        stats: Stats {
            run,
            resumed,
            ..Stats::default()
        },
        on_event,
    };

    for source in &config.sources {
        let url = urlnorm::normalize(&source.url, None).expect("validated in Config");
        crawler.store.enqueue(run, url.as_str(), &source.name, 0)?;
    }
    for page in crawler.store.pending(run)? {
        if !config.sources.iter().any(|s| s.name == page.source) {
            continue;
        }
        let Ok(url) = Url::parse(&page.url) else {
            continue;
        };
        let task = Task {
            url: page.url,
            source: page.source,
            depth: page.depth,
            attempts: page.attempts,
        };
        crawler.frontier.push(&urlnorm::host(&url), task, now);
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<Done>();
    let mut tasks = JoinSet::new();
    let limit = config.crawler.max_concurrency;

    loop {
        // Start as much work as the concurrency limit and the schedule allow.
        let mut wake = None;
        while crawler.in_flight < limit && !crawler.fetch_budget_spent(options.max_fetches) {
            match crawler.next_job(Instant::now())? {
                Step::Start(job) => {
                    crawler.in_flight += 1;
                    let clients = Arc::clone(&clients);
                    let tx = tx.clone();
                    let max_body = config.crawler.max_body_bytes;
                    let token = config.crawler.product_token.clone();
                    if matches!(job, Job::Page { .. }) {
                        crawler.stats.requests += 1;
                    }
                    tasks.spawn(async move {
                        let done = match job {
                            Job::Robots { host, origin } => {
                                let fetched = fetch::fetch_robots(&clients, &origin, &token).await;
                                Done::Robots {
                                    host,
                                    origin,
                                    fetched,
                                }
                            }
                            Job::Page { host, task } => {
                                let url = Url::parse(&task.url).expect("stored URLs are valid");
                                let result = fetch::fetch_page(&clients, &url, max_body).await;
                                Done::Page {
                                    host,
                                    task,
                                    url,
                                    result,
                                }
                            }
                        };
                        let _ = tx.send(done);
                    });
                }
                Step::Wait(at) => {
                    wake = at;
                    break;
                }
                Step::Again => {}
            }
        }

        if crawler.in_flight == 0 && wake.is_none() {
            break; // nothing in flight and nothing scheduled: the run is complete or the budget is spent
        }

        tokio::select! {
            Some(done) = rx.recv() => {
                crawler.in_flight -= 1;
                crawler.handle(done)?;
            }
            Some(joined) = tasks.join_next(), if !tasks.is_empty() => {
                if let Err(e) = joined {
                    return Err(Error::Task(e.to_string()));
                }
            }
            _ = sleep_until(wake.unwrap_or_else(Instant::now)), if wake.is_some() => {}
        }
    }

    let complete = crawler.frontier.pending() == 0 && crawler.waiting_for_robots.is_empty();
    if complete {
        crawler.store.finish_run(run)?;
    }
    crawler.stats.finished = complete;
    Ok(crawler.stats)
}

impl<F: FnMut(&Event)> Crawler<'_, F> {
    fn fetch_budget_spent(&self, max: Option<u64>) -> bool {
        max.is_some_and(|m| self.stats.requests >= m)
    }

    fn next_job(&mut self, now: Instant) -> Result<Step> {
        let (host, task) = match self.frontier.next(now) {
            Next::Ready { host, task } => (host, task),
            Next::WaitUntil(at) => return Ok(Step::Wait(Some(at))),
            Next::Idle => return Ok(Step::Wait(None)),
        };
        let url = Url::parse(&task.url).expect("stored URLs are valid");
        let origin = urlnorm::origin(&url);

        let refresh = match self.robots.get(&origin) {
            None => true,
            Some(RobotsState::Known { fetched, .. }) => {
                now.duration_since(*fetched) >= robots::MAX_CACHE_AGE
            }
            Some(RobotsState::Unreachable { until, .. }) => now >= *until,
            Some(RobotsState::Fetching) => false,
        };
        if refresh {
            self.robots.insert(origin.clone(), RobotsState::Fetching);
            self.waiting_for_robots
                .entry(origin.clone())
                .or_default()
                .push((host.clone(), task));
            // The host stays busy while its robots.txt is fetched, so the request counts
            // against the same politeness budget as a page.
            return Ok(Step::Start(Job::Robots { host, origin }));
        }

        let reason = match self.robots.get(&origin) {
            Some(RobotsState::Known { rules, .. }) if rules.allowed(url.as_str()) => {
                return Ok(Step::Start(Job::Page { host, task }));
            }
            Some(RobotsState::Known { rules, .. }) => rules.block_reason(),
            Some(RobotsState::Unreachable { reason, .. }) => {
                format!("robots.txt unreachable ({reason})")
            }
            // An origin belongs to exactly one host, and a host fetching its robots.txt is
            // busy, so its tasks cannot be handed out meanwhile.
            Some(RobotsState::Fetching) | None => {
                unreachable!("robots.txt state is settled before tasks run")
            }
        };
        // No request was made, so the host may be used again immediately.
        self.frontier.release(&host, now, Duration::ZERO);
        self.commit(
            &task.url,
            Some(EventKind::Blocked),
            Outcome {
                detail: Some(reason),
                ..Outcome::default()
            },
        )?;
        Ok(Step::Again)
    }

    fn handle(&mut self, done: Done) -> Result<()> {
        let now = Instant::now();
        match done {
            Done::Robots {
                host,
                origin,
                fetched,
            } => {
                let parked = self.waiting_for_robots.remove(&origin).unwrap_or_default();
                match fetched {
                    robots::Fetched::Rules(rules) => {
                        if let Some(delay) = rules.crawl_delay() {
                            let delay = delay
                                .min(self.config.max_crawl_delay())
                                .max(self.config.per_host_delay());
                            self.frontier.set_delay(&host, delay, now);
                        }
                        self.robots.insert(
                            origin,
                            RobotsState::Known {
                                rules,
                                fetched: now,
                            },
                        );
                    }
                    robots::Fetched::Unreachable { reason } => {
                        self.robots.insert(
                            origin,
                            RobotsState::Unreachable {
                                until: now + ROBOTS_RETRY,
                                reason,
                            },
                        );
                    }
                }
                for (task_host, task) in parked {
                    self.frontier.push_front(&task_host, task, now);
                }
                let delay = self.frontier.delay(&host);
                self.frontier.release(&host, now, delay);
            }
            Done::Page {
                host,
                task,
                url,
                result,
            } => {
                let wait = self.handle_page(&host, task, &url, result, now)?;
                self.frontier.release(&host, now, wait);
            }
        }
        Ok(())
    }

    /// Records the page outcome and returns how long the host must rest before its next request.
    fn handle_page(
        &mut self,
        host: &str,
        mut task: Task,
        url: &Url,
        result: PageResult,
        now: Instant,
    ) -> Result<Duration> {
        let delay = self.frontier.delay(host);
        match result {
            PageResult::Body {
                status,
                is_html,
                body,
                etag,
                last_modified,
            } => {
                self.stats.bytes += body.len() as u64;
                let (title, fingerprint_source, links) = if is_html {
                    let text = String::from_utf8_lossy(&body);
                    let extract = html::extract(&text, url, &self.config.crawler.product_token);
                    let source = extract.fingerprint_source();
                    (extract.title, source, extract.links)
                } else {
                    (None, body.clone(), Vec::new())
                };
                let outcome = Outcome {
                    status: Some(status),
                    title,
                    hash: Some(blake3::hash(&fingerprint_source).to_hex().to_string()),
                    etag,
                    last_modified,
                    bytes: Some(body.len() as i64),
                    detail: None,
                };
                self.commit(&task.url, None, outcome)?;
                self.follow_links(&task, url, links, now)?;
                Ok(delay)
            }
            PageResult::Redirect { status, location } => {
                let detail = location
                    .as_ref()
                    .map(|l| l.to_string())
                    .unwrap_or_else(|| "missing Location".into());
                let kind = if location.is_some() {
                    EventKind::Redirect
                } else {
                    EventKind::Error
                };
                self.commit(
                    &task.url,
                    Some(kind),
                    Outcome {
                        status: Some(status),
                        detail: Some(detail),
                        ..Outcome::default()
                    },
                )?;
                if let Some(target) = location {
                    // The target is crawled as its own page and passes robots.txt for its own origin.
                    self.enqueue(&target, &task.source, task.depth, now)?;
                }
                Ok(delay)
            }
            PageResult::SlowDown {
                status,
                retry_after,
            } => {
                let wait = retry_after.unwrap_or(SLOW_DOWN_DEFAULT).max(delay);
                if wait > MAX_RETRY_AFTER || task.attempts + 1 >= self.config.crawler.max_attempts {
                    let detail = format!(
                        "HTTP {status}, gave up after {} attempts",
                        task.attempts + 1
                    );
                    self.commit(
                        &task.url,
                        Some(EventKind::Error),
                        Outcome {
                            status: Some(status),
                            detail: Some(detail),
                            ..Outcome::default()
                        },
                    )?;
                    return Ok(wait.min(MAX_RETRY_AFTER));
                }
                self.retry(host, &mut task, now)?;
                Ok(wait)
            }
            PageResult::Status(status @ (404 | 410)) => {
                self.commit(
                    &task.url,
                    Some(EventKind::Gone),
                    Outcome {
                        status: Some(status),
                        ..Outcome::default()
                    },
                )?;
                Ok(delay)
            }
            PageResult::Status(status) if status >= 500 => {
                self.retry_or_fail(host, task, Some(status), format!("HTTP {status}"), now)
            }
            PageResult::Status(status) => {
                self.commit(
                    &task.url,
                    Some(EventKind::Error),
                    Outcome {
                        status: Some(status),
                        detail: Some(format!("HTTP {status}")),
                        ..Outcome::default()
                    },
                )?;
                Ok(delay)
            }
            PageResult::TooLarge { status, limit } => {
                let detail = format!("body larger than {limit} bytes");
                self.commit(
                    &task.url,
                    Some(EventKind::Error),
                    Outcome {
                        status: Some(status),
                        detail: Some(detail),
                        ..Outcome::default()
                    },
                )?;
                Ok(delay)
            }
            PageResult::Network(reason) => self.retry_or_fail(host, task, None, reason, now),
        }
    }

    fn retry_or_fail(
        &mut self,
        host: &str,
        mut task: Task,
        status: Option<u16>,
        reason: String,
        now: Instant,
    ) -> Result<Duration> {
        let delay = self.frontier.delay(host);
        if task.attempts + 1 >= self.config.crawler.max_attempts {
            let detail = format!("{reason}, gave up after {} attempts", task.attempts + 1);
            self.commit(
                &task.url,
                Some(EventKind::Error),
                Outcome {
                    status,
                    detail: Some(detail),
                    ..Outcome::default()
                },
            )?;
            return Ok(delay);
        }
        self.retry(host, &mut task, now)?;
        Ok(delay.max(self.config.retry_backoff() * task.attempts))
    }

    fn retry(&mut self, host: &str, task: &mut Task, now: Instant) -> Result<()> {
        task.attempts += 1;
        self.store.record_attempt(&task.url, task.attempts)?;
        self.frontier.push_front(host, task.clone(), now);
        Ok(())
    }

    fn follow_links(
        &mut self,
        parent: &Task,
        parent_url: &Url,
        links: Vec<Url>,
        now: Instant,
    ) -> Result<()> {
        let Some(source) = self.config.sources.iter().find(|s| s.name == parent.source) else {
            return Ok(());
        };
        if parent.depth >= source.max_depth {
            return Ok(());
        }
        let parent_host = parent_url.host_str();
        for link in links.into_iter().filter(|l| l.host_str() == parent_host) {
            self.enqueue(&link, &parent.source, parent.depth + 1, now)?;
        }
        Ok(())
    }

    fn enqueue(&mut self, url: &Url, source: &str, depth: u32, now: Instant) -> Result<()> {
        if self.store.enqueue(self.run, url.as_str(), source, depth)? {
            let task = Task {
                url: url.to_string(),
                source: source.to_string(),
                depth,
                attempts: 0,
            };
            self.frontier.push(&urlnorm::host(url), task, now);
        }
        Ok(())
    }

    fn commit(&mut self, url: &str, kind: Option<EventKind>, outcome: Outcome) -> Result<()> {
        let event = self.store.complete(self.run, url, kind, outcome)?;
        self.stats.events += 1;
        if matches!(event.kind, EventKind::Changed) {
            self.stats.changed += 1;
        }
        (self.on_event)(&event);
        Ok(())
    }
}

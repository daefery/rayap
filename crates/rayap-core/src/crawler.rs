//! The crawl loop.
//!
//! One coordinator owns the frontier, the robots.txt cache and the store; fetches run as
//! tasks and report back over a channel. Because only the coordinator touches state, the
//! scheduling rules hold without locks:
//!
//! - every request, including robots.txt and each redirect hop of a robots.txt fetch, is
//!   handed out by the frontier, so a host never has two requests in flight and always gets
//!   its delay between requests;
//! - no page is requested before its origin's robots.txt is known;
//! - a host told to wait (Retry-After, backoff) is held for the full time, and the hold is
//!   stored so a restart does not contact it early.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::{Duration, Instant, sleep_until};
use url::Url;

use crate::fetch::{self, Clients, PageResult, RobotsResult};
use crate::frontier::{Frontier, Next};
use crate::robots::{self, Rules};
use crate::store::{Event, EventKind, NewPage, Outcome, Store, unix_now_ms};
use crate::{Config, Error, Result, urlnorm};

/// Delay used for 429/503 without a usable Retry-After.
const SLOW_DOWN_DEFAULT: Duration = Duration::from_secs(60);
/// RFC 9309 §2.3.1.2: follow at least five consecutive redirects for robots.txt.
const MAX_ROBOTS_HOPS: u32 = 5;
/// Redirects followed from one source URL before the chain is reported as an error.
const MAX_PAGE_HOPS: u32 = 10;
/// In-memory stand-in for a hold too far away to add to the clock. The stored deadline is
/// kept exact; this only decides that the host is not contacted in this run.
const FAR_FUTURE: Duration = Duration::from_secs(100 * 365 * 24 * 60 * 60);

fn deadline(now: Instant, wait: Duration) -> Instant {
    now.checked_add(wait).unwrap_or_else(|| now + FAR_FUTURE)
}

#[derive(Clone, Default)]
pub struct Options {
    /// Stop after this many page requests (robots.txt requests not counted). Useful for tests
    /// and trial runs; an interrupted run resumes next time.
    pub max_fetches: Option<u64>,
    /// Replaces system DNS (benchmarks point simulated host names at a local server).
    pub resolver: Option<Arc<dyn reqwest::dns::Resolve>>,
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("max_fetches", &self.max_fetches)
            .field("resolver", &self.resolver.as_ref().map(|_| "custom"))
            .finish()
    }
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
    /// Time the coordinator spent working (scheduling and recording), as opposed to waiting.
    /// Close to the run's wall time means the coordinator is the bottleneck.
    pub coordinator_busy: Duration,
}

/// A page waiting to be fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Page {
    url: String,
    source: String,
    depth: u32,
    hops: u32,
    attempts: u32,
}

/// One unit of work in the frontier: a request to the queue's host.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Work {
    Page(Page),
    /// Fetch `url` on behalf of `origin`'s robots.txt (the first request or a redirect hop).
    Robots {
        origin: String,
        url: String,
        hops: u32,
    },
}

enum RobotsState {
    Fetching,
    Known {
        rules: Rules,
        fetched: Instant,
    },
    /// Pages wait while robots.txt is retried with backoff; after `max_attempts` failures in
    /// this run they are reported as blocked.
    Unreachable {
        until: Instant,
        reason: String,
        failures: u32,
    },
}

enum Job {
    Robots {
        origin: String,
        url: String,
        hops: u32,
    },
    Page(Page),
}

enum Step {
    Start {
        host: String,
        job: Job,
    },
    /// Stop starting work for now; the next host becomes eligible at this time, if any.
    Wait(Option<Instant>),
    /// An item was settled without a request; ask again.
    Again,
}

enum Done {
    Robots {
        host: String,
        origin: String,
        hops: u32,
        result: RobotsResult,
    },
    Page {
        host: String,
        page: Page,
        result: PageResult,
    },
}

struct Crawler<'a, F: FnMut(&Event)> {
    config: &'a Config,
    store: &'a mut Store,
    run: i64,
    frontier: Frontier<Work>,
    robots: HashMap<String, RobotsState>,
    /// Pages parked until their origin's robots.txt is known.
    waiting_for_robots: HashMap<String, Vec<Page>>,
    /// robots.txt failures so far in this run, for origins being fetched.
    robots_failures: HashMap<String, u32>,
    /// Crawl-delay per host, per origin; a host uses the largest among its origins.
    origin_delays: HashMap<String, HashMap<String, Duration>>,
    seed_hosts: HashMap<String, String>,
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
    let clients = Arc::new(Clients::new(config, options.resolver.clone())?);
    let sources: Vec<&str> = config.sources.iter().map(|s| s.name.as_str()).collect();
    let (run, resumed) = store.begin_run(&sources)?;
    let mut seed_hosts = HashMap::new();
    for source in &config.sources {
        let url = urlnorm::normalize(&source.url, None).expect("validated in Config");
        seed_hosts.insert(source.name.clone(), urlnorm::host(&url));
        store.enqueue(
            run,
            &NewPage {
                url: url.to_string(),
                source: source.name.clone(),
                depth: 0,
                hops: 0,
            },
        )?;
    }
    let mut crawler = Crawler {
        config,
        store,
        run,
        frontier: Frontier::new(config.per_host_delay()),
        robots: HashMap::new(),
        waiting_for_robots: HashMap::new(),
        robots_failures: HashMap::new(),
        origin_delays: HashMap::new(),
        seed_hosts,
        in_flight: 0,
        stats: Stats {
            run,
            resumed,
            ..Stats::default()
        },
        on_event,
    };

    // Sample both clocks together so a stored hold is not shortened by setup time.
    let (now, now_ms) = (Instant::now(), unix_now_ms());
    for (host, until_ms) in crawler.store.active_holds()? {
        let wait = Duration::from_millis(u64::try_from(until_ms - now_ms).unwrap_or(0));
        crawler.frontier.hold(&host, deadline(now, wait), now);
    }
    // robots.txt failures earlier in this run keep counting towards max_attempts.
    for (origin, failures) in crawler.store.robots_failures(run)? {
        crawler.robots.insert(
            origin,
            RobotsState::Unreachable {
                until: now,
                reason: "unreachable earlier in this run".into(),
                failures,
            },
        );
    }
    for p in crawler.store.pending(run)? {
        if !config.sources.iter().any(|s| s.name == p.source) {
            continue;
        }
        let Ok(url) = Url::parse(&p.url) else {
            continue;
        };
        let page = Page {
            url: p.url,
            source: p.source,
            depth: p.depth,
            hops: p.hops,
            attempts: p.attempts,
        };
        crawler
            .frontier
            .push(&urlnorm::host(&url), Work::Page(page), now);
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<Done>();
    let mut tasks = JoinSet::new();
    let limit = config.crawler.max_concurrency;

    loop {
        // Start as much work as the concurrency limit and the schedule allow.
        let busy_from = Instant::now();
        let mut wake = None;
        while crawler.in_flight < limit && !crawler.fetch_budget_spent(options.max_fetches) {
            match crawler.next_step(Instant::now())? {
                Step::Start { host, job } => {
                    crawler.in_flight += 1;
                    if matches!(job, Job::Page(_)) {
                        crawler.stats.requests += 1;
                    }
                    let clients = Arc::clone(&clients);
                    let tx = tx.clone();
                    let max_body = config.crawler.max_body_bytes;
                    let token = config.crawler.product_token.clone();
                    tasks.spawn(async move {
                        let done = match job {
                            Job::Robots { origin, url, hops } => {
                                let result = fetch::fetch_robots(&clients, &url).await;
                                Done::Robots {
                                    host,
                                    origin,
                                    hops,
                                    result,
                                }
                            }
                            Job::Page(page) => {
                                let url = Url::parse(&page.url).expect("stored URLs are valid");
                                let result =
                                    fetch::fetch_page(&clients, &url, max_body, &token).await;
                                Done::Page { host, page, result }
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

        let too_far = wake.is_some_and(|at| at > Instant::now() + config.max_wait());
        if crawler.in_flight == 0
            && (wake.is_none() || too_far || crawler.fetch_budget_spent(options.max_fetches))
        {
            // Nothing in flight and nothing to do soon. Held hosts keep their stored holds
            // and their pages stay pending, so the next run picks them up.
            break;
        }

        crawler.stats.coordinator_busy += busy_from.elapsed();
        tokio::select! {
            Some(done) = rx.recv() => {
                let started = Instant::now();
                crawler.in_flight -= 1;
                crawler.handle(done)?;
                // Drain whatever else has finished before scheduling again, so a busy
                // coordinator makes one scheduling pass per batch rather than per response.
                while let Ok(done) = rx.try_recv() {
                    crawler.in_flight -= 1;
                    crawler.handle(done)?;
                }
                crawler.stats.coordinator_busy += started.elapsed();
            }
            Some(joined) = tasks.join_next(), if !tasks.is_empty() => {
                if let Err(e) = joined {
                    return Err(Error::Task(e.to_string()));
                }
            }
            _ = sleep_until(wake.unwrap_or_else(Instant::now)), if wake.is_some() => {}
        }
    }

    let complete =
        crawler.frontier.pending() == 0 && crawler.waiting_for_robots.values().all(Vec::is_empty);
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

    fn next_step(&mut self, now: Instant) -> Result<Step> {
        let (host, work) = match self.frontier.next(now) {
            Next::Ready { host, item } => (host, item),
            Next::WaitUntil(at) => return Ok(Step::Wait(Some(at))),
            Next::Idle => return Ok(Step::Wait(None)),
        };
        let page = match work {
            Work::Robots { origin, url, hops } => {
                return Ok(Step::Start {
                    host,
                    job: Job::Robots { origin, url, hops },
                });
            }
            Work::Page(page) => page,
        };
        let url = Url::parse(&page.url).expect("stored URLs are valid");
        let origin = urlnorm::origin(&url);

        let state = self.robots.get(&origin);
        let max_attempts = self.config.crawler.max_attempts;
        let refresh = match state {
            None => true,
            Some(RobotsState::Known { fetched, .. }) => {
                now.duration_since(*fetched) >= robots::MAX_CACHE_AGE
            }
            Some(RobotsState::Unreachable {
                until, failures, ..
            }) => now >= *until && *failures < max_attempts,
            Some(RobotsState::Fetching) => false,
        };
        if refresh {
            let failures = match state {
                Some(RobotsState::Unreachable { failures, .. }) => *failures,
                _ => 0,
            };
            self.robots_failures.insert(origin.clone(), failures);
            self.robots.insert(origin.clone(), RobotsState::Fetching);
            self.waiting_for_robots
                .entry(origin.clone())
                .or_default()
                .push(page);
            let robots_url = format!("{origin}/robots.txt");
            return Ok(Step::Start {
                host,
                job: Job::Robots {
                    origin,
                    url: robots_url,
                    hops: 0,
                },
            });
        }

        match state {
            Some(RobotsState::Known { rules, .. })
                if rules.allowed(&url[url::Position::BeforePath..]) =>
            {
                Ok(Step::Start {
                    host,
                    job: Job::Page(page),
                })
            }
            Some(RobotsState::Known { rules, .. }) => {
                let reason = rules.block_reason();
                // No request was made, so the host may be used again immediately.
                self.frontier.release(&host, now, Duration::ZERO);
                self.commit(
                    &page.url,
                    Some(EventKind::Blocked),
                    Outcome {
                        detail: Some(reason),
                        ..Outcome::default()
                    },
                    Vec::new(),
                )?;
                Ok(Step::Again)
            }
            Some(RobotsState::Unreachable {
                reason, failures, ..
            }) if *failures >= self.config.crawler.max_attempts => {
                let detail = format!("robots.txt unreachable after {failures} attempts ({reason})");
                self.frontier.release(&host, now, Duration::ZERO);
                self.commit(
                    &page.url,
                    Some(EventKind::Blocked),
                    Outcome {
                        detail: Some(detail),
                        ..Outcome::default()
                    },
                    Vec::new(),
                )?;
                Ok(Step::Again)
            }
            Some(RobotsState::Unreachable { .. } | RobotsState::Fetching) => {
                // An unreachable robots.txt is never treated as permission: the page waits
                // until robots.txt is retried.
                self.waiting_for_robots
                    .entry(origin)
                    .or_default()
                    .push(page);
                self.frontier.release(&host, now, Duration::ZERO);
                Ok(Step::Again)
            }
            None => unreachable!("handled by refresh"),
        }
    }

    fn handle(&mut self, done: Done) -> Result<()> {
        let now = Instant::now();
        // Store the host's ordinary delay before recording anything, so that whatever happens
        // next (a crash, a panicking event callback) a restart still waits at least this long.
        let host = match &done {
            Done::Robots { host, .. } | Done::Page { host, .. } => host.clone(),
        };
        let delay = self.frontier.delay(&host);
        self.hold(&host, now, delay)?;
        match done {
            Done::Robots {
                host,
                origin,
                hops,
                result,
            } => self.handle_robots(&host, origin, hops, result, now),
            Done::Page { host, page, result } => {
                let wait = self.handle_page(&host, page, result, now)?;
                self.release(&host, now, wait)
            }
        }
    }

    fn handle_robots(
        &mut self,
        host: &str,
        origin: String,
        hops: u32,
        result: RobotsResult,
        now: Instant,
    ) -> Result<()> {
        let delay = self.frontier.delay(host);
        let fetched = match result {
            RobotsResult::Redirect(Some(target)) if hops < MAX_ROBOTS_HOPS => {
                // The next hop is queued on the target's host, so it waits its turn there.
                let work = Work::Robots {
                    origin,
                    url: target.to_string(),
                    hops: hops + 1,
                };
                self.frontier.push_front(&urlnorm::host(&target), work, now);
                return self.release(host, now, delay);
            }
            RobotsResult::Redirect(_) => {
                // Too many redirects (or no Location): treat as unavailable, RFC 9309 §2.3.1.2.
                robots::Fetched::Rules(Rules::AllowAll)
            }
            RobotsResult::Response {
                status,
                body,
                retry_after,
            } => {
                if let Some(wait) = retry_after {
                    self.hold(host, now, wait)?;
                }
                robots::interpret(status, &body, &self.config.crawler.product_token)
            }
            RobotsResult::Network(reason) => robots::Fetched::Unreachable {
                reason: format!("robots.txt: {reason}"),
            },
        };

        let origin_host = Url::parse(&origin)
            .map(|u| urlnorm::host(&u))
            .unwrap_or_else(|_| host.to_string());
        match fetched {
            robots::Fetched::Rules(rules) => {
                self.store.clear_robots_failures(self.run, &origin)?;
                let crawl_delay = rules.crawl_delay(self.config.max_crawl_delay());
                let per_host = self.origin_delays.entry(origin_host.clone()).or_default();
                match crawl_delay {
                    Some(d) => per_host.insert(origin.clone(), d),
                    None => per_host.remove(&origin),
                };
                self.update_host_delay(&origin_host, now);
                self.robots.insert(
                    origin.clone(),
                    RobotsState::Known {
                        rules,
                        fetched: now,
                    },
                );
            }
            robots::Fetched::Unreachable { reason } => {
                let failures = self.robots_failures.remove(&origin).unwrap_or(0) + 1;
                let wait = self
                    .frontier
                    .delay(&origin_host)
                    .max(self.config.retry_backoff() * failures);
                // Hold first, as for page retries: a crash between the two writes must not
                // leave a recorded failure without its backoff.
                self.hold(&origin_host, now, wait)?;
                self.store
                    .record_robots_failure(self.run, &origin, failures)?;
                self.robots.insert(
                    origin.clone(),
                    RobotsState::Unreachable {
                        until: deadline(now, wait),
                        reason,
                        failures,
                    },
                );
            }
        }
        for page in self
            .waiting_for_robots
            .remove(&origin)
            .unwrap_or_default()
            .into_iter()
            .rev()
        {
            self.frontier
                .push_front(&origin_host, Work::Page(page), now);
        }
        if origin_host != host {
            // robots.txt was served from another host after a redirect. The origin's host was
            // last contacted at the first hop; its (possibly new) Crawl-delay counts from now.
            let origin_delay = self.frontier.delay(&origin_host);
            self.hold(&origin_host, now, origin_delay)?;
        }
        let delay = self.frontier.delay(host);
        self.release(host, now, delay)
    }

    /// A host's delay is the configured delay or the largest Crawl-delay of its origins.
    fn update_host_delay(&mut self, host: &str, now: Instant) {
        let largest = self
            .origin_delays
            .get(host)
            .and_then(|o| o.values().max().copied());
        let delay = largest
            .unwrap_or_default()
            .max(self.config.per_host_delay());
        self.frontier.set_delay(host, delay, now);
    }

    /// Keeps `host` from being contacted for `wait` after `now`, in memory and in the store,
    /// so a restart honours it too. Earlier holds that end later are kept.
    fn hold(&mut self, host: &str, now: Instant, wait: Duration) -> Result<()> {
        self.frontier.hold(host, deadline(now, wait), now);
        let ms = i64::try_from(wait.as_millis()).unwrap_or(i64::MAX);
        self.store.hold_host(host, unix_now_ms().saturating_add(ms))
    }

    /// Ends a request to `host`: the host may be contacted again after `wait` (or later if
    /// held). The time is stored so a restart cannot shorten it.
    fn release(&mut self, host: &str, now: Instant, wait: Duration) -> Result<()> {
        self.hold(host, now, wait)?;
        self.frontier.release(host, now, Duration::ZERO);
        Ok(())
    }

    /// Records the page outcome and returns how long the host must rest before its next request.
    fn handle_page(
        &mut self,
        host: &str,
        mut page: Page,
        result: PageResult,
        now: Instant,
    ) -> Result<Duration> {
        let delay = self.frontier.delay(host);
        match result {
            PageResult::Body {
                status,
                bytes,
                title,
                hash,
                links,
                etag,
                last_modified,
            } => {
                self.stats.bytes += bytes as u64;
                let outcome = Outcome {
                    status: Some(status),
                    title,
                    hash: Some(hash),
                    etag,
                    last_modified,
                    bytes: Some(bytes as i64),
                    detail: None,
                };
                let discovered = self.links_to_follow(&page, links);
                self.commit(&page.url, None, outcome, discovered)?;
                Ok(delay)
            }
            PageResult::Redirect { status, location } => {
                let (kind, detail, discovered) = match location {
                    Some(target) if page.hops < MAX_PAGE_HOPS => {
                        // The target is crawled as its own page: robots.txt and politeness for its host.
                        let next = NewPage {
                            url: target.to_string(),
                            source: page.source.clone(),
                            depth: page.depth,
                            hops: page.hops + 1,
                        };
                        (EventKind::Redirect, target.to_string(), vec![next])
                    }
                    Some(target) => (
                        EventKind::Error,
                        format!("more than {MAX_PAGE_HOPS} redirects (next: {target})"),
                        Vec::new(),
                    ),
                    None => (
                        EventKind::Error,
                        "redirect without a usable Location".into(),
                        Vec::new(),
                    ),
                };
                let outcome = Outcome {
                    status: Some(status),
                    detail: Some(detail),
                    ..Outcome::default()
                };
                self.commit(&page.url, Some(kind), outcome, discovered)?;
                Ok(delay)
            }
            PageResult::SlowDown {
                status,
                retry_after,
            } => {
                let wait = retry_after.unwrap_or(SLOW_DOWN_DEFAULT).max(delay);
                // The host is held for the full time the server asked for, whatever happens to this page.
                self.hold(host, now, wait)?;
                if page.attempts + 1 >= self.config.crawler.max_attempts {
                    let detail = format!(
                        "HTTP {status}, gave up after {} attempts",
                        page.attempts + 1
                    );
                    self.commit(
                        &page.url,
                        Some(EventKind::Error),
                        Outcome {
                            status: Some(status),
                            detail: Some(detail),
                            ..Outcome::default()
                        },
                        Vec::new(),
                    )?;
                } else {
                    self.retry(host, &mut page, now)?;
                }
                Ok(wait)
            }
            PageResult::Status(status @ (404 | 410)) => {
                self.commit(
                    &page.url,
                    Some(EventKind::Gone),
                    Outcome {
                        status: Some(status),
                        ..Outcome::default()
                    },
                    Vec::new(),
                )?;
                Ok(delay)
            }
            PageResult::Status(status) if status >= 500 => {
                self.retry_or_fail(host, page, Some(status), format!("HTTP {status}"), now)
            }
            PageResult::Status(status) => {
                self.commit(
                    &page.url,
                    Some(EventKind::Error),
                    Outcome {
                        status: Some(status),
                        detail: Some(format!("HTTP {status}")),
                        ..Outcome::default()
                    },
                    Vec::new(),
                )?;
                Ok(delay)
            }
            PageResult::TooLarge { status, limit } => {
                let detail = format!("body larger than {limit} bytes");
                self.commit(
                    &page.url,
                    Some(EventKind::Error),
                    Outcome {
                        status: Some(status),
                        detail: Some(detail),
                        ..Outcome::default()
                    },
                    Vec::new(),
                )?;
                Ok(delay)
            }
            PageResult::Network(reason) => self.retry_or_fail(host, page, None, reason, now),
        }
    }

    fn retry_or_fail(
        &mut self,
        host: &str,
        mut page: Page,
        status: Option<u16>,
        reason: String,
        now: Instant,
    ) -> Result<Duration> {
        let delay = self.frontier.delay(host);
        if page.attempts + 1 >= self.config.crawler.max_attempts {
            let detail = format!("{reason}, gave up after {} attempts", page.attempts + 1);
            self.commit(
                &page.url,
                Some(EventKind::Error),
                Outcome {
                    status,
                    detail: Some(detail),
                    ..Outcome::default()
                },
                Vec::new(),
            )?;
            return Ok(delay);
        }
        let wait = delay.max(self.config.retry_backoff() * (page.attempts + 1));
        // Hold first: if the process stops between the two writes, the host still waits.
        self.hold(host, now, wait)?;
        self.retry(host, &mut page, now)?;
        Ok(wait)
    }

    fn retry(&mut self, host: &str, page: &mut Page, now: Instant) -> Result<()> {
        page.attempts += 1;
        self.store.record_attempt(&page.url, page.attempts)?;
        self.frontier
            .push_front(host, Work::Page(page.clone()), now);
        Ok(())
    }

    /// Links on the source's own host, while the source's depth allows.
    fn links_to_follow(&self, parent: &Page, links: Vec<Url>) -> Vec<NewPage> {
        let Some(source) = self.config.sources.iter().find(|s| s.name == parent.source) else {
            return Vec::new();
        };
        if parent.depth >= source.max_depth {
            return Vec::new();
        }
        let seed_host = self.seed_hosts.get(&parent.source).map(String::as_str);
        links
            .into_iter()
            .filter(|l| l.host_str() == seed_host)
            .map(|l| NewPage {
                url: l.to_string(),
                source: parent.source.clone(),
                depth: parent.depth + 1,
                hops: 0,
            })
            .collect()
    }

    /// Commits the outcome, its event and the pages it led to atomically, then queues the
    /// pages that are new to this run and reports the event.
    fn commit(
        &mut self,
        url: &str,
        kind: Option<EventKind>,
        outcome: Outcome,
        discovered: Vec<NewPage>,
    ) -> Result<()> {
        let (event, added) = self
            .store
            .complete(self.run, url, kind, outcome, discovered)?;
        let now = Instant::now();
        for p in added {
            let Ok(parsed) = Url::parse(&p.url) else {
                continue;
            };
            let page = Page {
                url: p.url,
                source: p.source,
                depth: p.depth,
                hops: p.hops,
                attempts: 0,
            };
            self.frontier
                .push(&urlnorm::host(&parsed), Work::Page(page), now);
        }
        self.stats.events += 1;
        if matches!(event.kind, EventKind::Changed) {
            self.stats.changed += 1;
        }
        (self.on_event)(&event);
        Ok(())
    }
}

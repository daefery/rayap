//! The crawl loop.
//!
//! Hosts are split across `shards` coordinators by a hash of the host name. Each coordinator
//! owns the frontier, robots.txt cache and in-flight count for its hosts; fetches run as
//! tasks and report back over a channel. One writer task owns the store (`writer.rs`).
//! Because a host belongs to exactly one coordinator and only that coordinator touches its
//! state, the scheduling rules hold without locks:
//!
//! - every request, including robots.txt and each redirect hop of a robots.txt fetch, is
//!   handed out by the host's frontier, so a host never has two requests in flight and
//!   always gets its delay between requests;
//! - no page is requested before its origin's robots.txt is known;
//! - a host told to wait (Retry-After, backoff) is held for the full time, and the hold is
//!   stored so a restart does not contact it early.
//!
//! A page discovered on one shard whose host belongs to another is handed over through a
//! channel; it is already recorded in the store by then, so a crash loses nothing.

use std::collections::HashMap;
use std::sync::Arc;

use std::sync::atomic::Ordering;
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio::time::{Duration, Instant, sleep_until};
use url::Url;

use crate::config::SourceKind;
use crate::fetch::{self, Clients, Extractor, PageResult, RobotsResult};
use crate::frontier::{Frontier, Next};
use crate::robots::{self, Rules};
use crate::store::{Event, EventKind, NewPage, Outcome, Plan, Store, unix_now_ms};
use crate::writer::{self, Writer};
use crate::{Config, Error, Result, urlnorm};

/// Delay used for 429/503 without a usable Retry-After.
const SLOW_DOWN_DEFAULT: Duration = Duration::from_secs(60);
/// Hold on a host that answered a page with a challenge (a 200 that is really a "slow down").
/// Longer than a 429 default: the host said nothing about when to come back.
const CHALLENGE_HOLD: Duration = Duration::from_secs(5 * 60);
/// RFC 9309 §2.3.1.2: follow at least five consecutive redirects for robots.txt.
const MAX_ROBOTS_HOPS: u32 = 5;
/// Redirects followed from one source URL before the chain is reported as an error.
const MAX_PAGE_HOPS: u32 = 10;
/// Responses recorded per transaction at most. Bounds how much work a crash can undo and
/// how long events wait before delivery.
const MAX_BATCH: usize = 512;
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
    /// Running total of coordinator busy time in microseconds, for live sampling.
    pub busy_micros: Option<Arc<std::sync::atomic::AtomicU64>>,
    /// Visit only the pages most due by learned change rate (plus never-fetched ones),
    /// instead of every known page. Ignored when resuming an unfinished run.
    pub plan: Option<Plan>,
    /// Coordinators to split hosts across. 0 or 1 means one coordinator, as before.
    pub shards: usize,
}

impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("max_fetches", &self.max_fetches)
            .field("resolver", &self.resolver.as_ref().map(|_| "custom"))
            .field("busy_micros", &self.busy_micros.is_some())
            .field("plan", &self.plan)
            .field("shards", &self.shards)
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
    /// Known pages the plan left for a later run.
    pub deferred: u64,
    /// Time the coordinators spent working (scheduling and recording), as opposed to waiting,
    /// summed over shards. Close to shards x wall time means they are the bottleneck.
    pub coordinator_busy: Duration,
    /// Pages handed from one shard to another because a link crossed hosts.
    pub handed_over: u64,
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

fn country_of(source: &crate::config::Source) -> crate::jobs::Country {
    if source.country.eq_ignore_ascii_case("indonesia") {
        crate::jobs::Country::indonesia()
    } else {
        crate::jobs::Country::named(&source.country, Vec::new())
    }
}

/// One coordinator: the hosts whose hash lands on it, and nothing else.
struct Crawler<'a> {
    config: &'a Config,
    writer: Writer,
    run: i64,
    frontier: Frontier<Work>,
    robots: HashMap<String, RobotsState>,
    /// Pages parked until their origin's robots.txt is known.
    waiting_for_robots: HashMap<String, Vec<Page>>,
    /// robots.txt failures so far in this run, for origins being fetched.
    robots_failures: HashMap<String, u32>,
    /// Crawl-delay per host, per origin; a host uses the largest among its origins.
    origin_delays: HashMap<String, HashMap<String, Duration>>,
    seed_hosts: &'a HashMap<String, String>,
    /// How each source's pages are read, by source name, and whether they are rendered.
    extractors: &'a HashMap<String, (Extractor, bool)>,
    in_flight: usize,
    stats: Stats,
    /// Events of the batch being recorded, delivered once it is committed.
    pending_events: Vec<Event>,
    /// Which shard owns a host, and the channel to it.
    routing: Routing,
    /// Pages discovered here that belong to another shard, sent after the batch commits.
    outbound: Vec<NewPage>,
    /// Batches sent to the writer whose commit has not been acknowledged yet, oldest first.
    /// The shard keeps scheduling while these are in flight; each is delivered when its
    /// acknowledgement arrives.
    in_commit: std::collections::VecDeque<InCommit>,
}

struct InCommit {
    ack: tokio::sync::oneshot::Receiver<Result<()>>,
    events: Vec<Event>,
    outbound: Vec<NewPage>,
}

/// How hosts map to shards, and the shared count of idle shards used to agree on the end.
#[derive(Clone)]
struct Routing {
    shards: usize,
    me: usize,
    handover: Vec<mpsc::UnboundedSender<NewPage>>,
    idle_shards: Arc<std::sync::atomic::AtomicUsize>,
}

impl Routing {
    fn owner(&self, host: &str) -> usize {
        if self.shards <= 1 {
            return 0;
        }
        // A stable, cheap hash: the same host always lands on the same shard within a run.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in host.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        (h % self.shards as u64) as usize
    }

    fn mine(&self, host: &str) -> bool {
        self.owner(host) == self.me
    }
}

/// Crawls every configured source once, resuming an unfinished run if there is one.
/// `on_event` is called after each event is durably stored, from the task that called
/// `crawl`, in commit order within a shard.
pub async fn crawl<F: FnMut(&Event)>(
    config: &Config,
    store: &mut Store,
    options: Options,
    mut on_event: F,
) -> Result<Stats> {
    let clients = Arc::new(Clients::new(config, options.resolver.clone())?);
    let sources: Vec<&str> = config.sources.iter().map(|s| s.name.as_str()).collect();
    let start = store.begin_run(&sources, options.plan.as_ref())?;
    let (run, resumed) = (start.run, start.resumed);
    let mut seed_hosts = HashMap::new();
    let mut extractors: HashMap<String, (Extractor, bool)> = HashMap::new();
    for source in &config.sources {
        let url = urlnorm::normalize(&source.url, None).expect("validated in Config");
        seed_hosts.insert(source.name.clone(), urlnorm::host(&url));
        let extractor = match source.kind {
            SourceKind::Page => Extractor::Page {
                product_token: config.crawler.product_token.clone(),
            },
            SourceKind::Tiktok => Extractor::Tiktok {
                max_comments: source.max_comments,
            },
            SourceKind::Company => Extractor::Company {
                home: url.clone(),
                product_token: config.crawler.product_token.clone(),
                country: Arc::new(country_of(source)),
            },
            SourceKind::Jobs => Extractor::Jobs {
                country: Arc::new(country_of(source)),
                max_jobs: source.max_jobs,
            },
            SourceKind::Ats => Extractor::Ats {
                country: Arc::new(country_of(source)),
            },
        };
        extractors.insert(source.name.clone(), (extractor, source.render));
        let seed = NewPage {
            url: url.to_string(),
            source: source.name.clone(),
            depth: 0,
            hops: 0,
            continuation: false,
        };
        if options.plan.is_some() {
            // The plan decides when a known seed is due; only new seeds are forced in.
            store.enqueue_if_unknown(run, &seed)?;
        } else {
            store.enqueue(run, &seed)?;
        }
    }
    let seed_hosts = Arc::new(seed_hosts);
    let extractors = Arc::new(extractors);

    // Sample both clocks together so a stored hold is not shortened by setup time.
    let (now, now_ms) = (Instant::now(), unix_now_ms());
    let holds: Vec<(String, Duration)> = store
        .active_holds()?
        .into_iter()
        .map(|(host, until_ms)| {
            let wait = Duration::from_millis(u64::try_from(until_ms - now_ms).unwrap_or(0));
            (host, wait)
        })
        .collect();
    // robots.txt failures earlier in this run keep counting towards max_attempts.
    let robots_failures = store.robots_failures(run)?;
    let mut pending: Vec<(String, Page)> = Vec::new();
    for p in store.pending(run)? {
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
        pending.push((urlnorm::host(&url), page));
    }

    // The store moves into the writer for the duration of the crawl and comes back after.
    let owned = std::mem::replace(store, Store::open_in_memory()?);
    let (writer, writer_task) = writer::spawn(owned);

    let shards = options.shards.max(1);
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Event>();
    let mut handover_tx = Vec::with_capacity(shards);
    let mut handover_rx = Vec::with_capacity(shards);
    for _ in 0..shards {
        let (tx, rx) = mpsc::unbounded_channel::<NewPage>();
        handover_tx.push(tx);
        handover_rx.push(rx);
    }
    let mut shard_tasks = JoinSet::new();
    let idle_shards = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let budget = Arc::new(FetchBudget::new(options.max_fetches));
    for (me, inbox) in handover_rx.into_iter().enumerate() {
        let routing = Routing {
            shards,
            me,
            handover: handover_tx.clone(),
            idle_shards: Arc::clone(&idle_shards),
        };
        let mut holds_here = Vec::new();
        for (host, wait) in &holds {
            if routing.mine(host) {
                holds_here.push((host.clone(), *wait));
            }
        }
        let mut pending_here = Vec::new();
        for (host, page) in &pending {
            if routing.mine(host) {
                pending_here.push((host.clone(), page.clone()));
            }
        }
        let shard = ShardSetup {
            config: config.clone(),
            writer: writer.clone(),
            run,
            seed_hosts: Arc::clone(&seed_hosts),
            extractors: Arc::clone(&extractors),
            clients: Arc::clone(&clients),
            holds: holds_here,
            robots_failures: robots_failures.clone(),
            pending: pending_here,
            routing,
            inbox,
            events: event_tx.clone(),
            budget: Arc::clone(&budget),
            busy_micros: options.busy_micros.clone(),
            start_now: now,
        };
        shard_tasks.spawn(run_shard(shard));
    }
    // Only shards hold senders now; the writer stops when the last shard drops its handle.
    drop(writer);
    drop(handover_tx);
    drop(event_tx);

    // Deliver events as shards commit them; collect shard results as they finish.
    let mut stats = Stats {
        run,
        resumed,
        deferred: start.deferred,
        ..Stats::default()
    };
    let mut complete = true;
    let mut shards_left = shards;
    loop {
        tokio::select! {
            Some(event) = event_rx.recv() => on_event(&event),
            joined = shard_tasks.join_next(), if shards_left > 0 => {
                match joined {
                    Some(Ok(Ok(outcome))) => {
                        stats.requests += outcome.stats.requests;
                        stats.events += outcome.stats.events;
                        stats.changed += outcome.stats.changed;
                        stats.bytes += outcome.stats.bytes;
                        stats.coordinator_busy += outcome.stats.coordinator_busy;
                        stats.handed_over += outcome.stats.handed_over;
                        complete &= outcome.complete;
                        shards_left -= 1;
                    }
                    Some(Ok(Err(e))) => return Err(e),
                    Some(Err(e)) => return Err(Error::Task(e.to_string())),
                    None => { shards_left = 0; }
                }
            }
            else => break,
        }
        if shards_left == 0 {
            // Shards are done; drain whatever events are still queued.
            while let Ok(event) = event_rx.try_recv() {
                on_event(&event);
            }
            break;
        }
    }

    let recovered = writer_task
        .await
        .map_err(|e| Error::Task(e.to_string()))??;
    *store = recovered;
    if complete {
        store.finish_run(run)?;
    }
    stats.finished = complete;
    Ok(stats)
}

struct ShardSetup {
    config: Config,
    writer: Writer,
    run: i64,
    seed_hosts: Arc<HashMap<String, String>>,
    extractors: Arc<HashMap<String, (Extractor, bool)>>,
    clients: Arc<Clients>,
    holds: Vec<(String, Duration)>,
    robots_failures: Vec<(String, u32)>,
    pending: Vec<(String, Page)>,
    routing: Routing,
    inbox: mpsc::UnboundedReceiver<NewPage>,
    events: mpsc::UnboundedSender<Event>,
    budget: Arc<FetchBudget>,
    busy_micros: Option<Arc<std::sync::atomic::AtomicU64>>,
    start_now: Instant,
}

/// `Options::max_fetches`, shared by all shards: a page request is started only if a unit
/// of budget can be taken first, so the total across shards never exceeds the limit.
struct FetchBudget {
    limit: Option<u64>,
    taken: std::sync::atomic::AtomicU64,
}

impl FetchBudget {
    fn new(limit: Option<u64>) -> Self {
        Self {
            limit,
            taken: std::sync::atomic::AtomicU64::new(0),
        }
    }

    fn spent(&self) -> bool {
        self.limit
            .is_some_and(|m| self.taken.load(Ordering::SeqCst) >= m)
    }

    /// Takes one unit; false if none is left.
    fn take(&self) -> bool {
        let Some(limit) = self.limit else {
            return true;
        };
        // Compare-and-swap loop: take one unit unless the limit is reached.
        let mut current = self.taken.load(Ordering::SeqCst);
        loop {
            if current >= limit {
                return false;
            }
            match self.taken.compare_exchange(
                current,
                current + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }
}

struct ShardOutcome {
    stats: Stats,
    complete: bool,
}

/// One coordinator's loop. Structurally the single-coordinator loop of before, plus an
/// inbox of pages handed over by other shards.
async fn run_shard(setup: ShardSetup) -> Result<ShardOutcome> {
    let ShardSetup {
        config,
        writer,
        run,
        seed_hosts,
        extractors,
        clients,
        holds,
        robots_failures,
        pending,
        routing,
        mut inbox,
        events,
        budget,
        busy_micros,
        start_now: now,
    } = setup;
    let mut crawler = Crawler {
        config: &config,
        writer,
        run,
        frontier: Frontier::new(config.per_host_delay()),
        robots: HashMap::new(),
        waiting_for_robots: HashMap::new(),
        robots_failures: HashMap::new(),
        origin_delays: HashMap::new(),
        seed_hosts: &seed_hosts,
        extractors: &extractors,
        in_flight: 0,
        stats: Stats::default(),
        pending_events: Vec::new(),
        routing,
        outbound: Vec::new(),
        in_commit: std::collections::VecDeque::new(),
    };
    for (host, wait) in holds {
        crawler.frontier.hold(&host, deadline(now, wait), now);
    }
    for (origin, failures) in robots_failures {
        crawler.robots.insert(
            origin,
            RobotsState::Unreachable {
                until: now,
                reason: "unreachable earlier in this run".into(),
                failures,
            },
        );
    }
    for (host, page) in pending {
        crawler.frontier.push(&host, Work::Page(page), now);
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<Done>();
    let mut tasks = JoinSet::new();
    // Each shard gets its share of the connection budget.
    let limit = (config.crawler.max_concurrency / crawler.routing.shards.max(1)).max(1);
    let idle_shards = Arc::clone(&crawler.routing.idle_shards);
    let mut marked_idle = false;

    loop {
        // Start as much work as the concurrency limit and the schedule allow.
        let busy_from = Instant::now();
        let mut wake = None;
        while crawler.in_flight < limit && !budget.spent() {
            match crawler.next_step(Instant::now()).await? {
                Step::Start { host, job } => {
                    if matches!(job, Job::Page(_)) {
                        if !budget.take() {
                            // Another shard spent the last unit between our check and now.
                            // Put the page back; this shard is done starting pages.
                            if let Job::Page(page) = job {
                                crawler.frontier.push_front(
                                    &host,
                                    Work::Page(page),
                                    Instant::now(),
                                );
                                crawler
                                    .frontier
                                    .release(&host, Instant::now(), Duration::ZERO);
                            }
                            break;
                        }
                        crawler.stats.requests += 1;
                    }
                    crawler.in_flight += 1;
                    let clients = Arc::clone(&clients);
                    let tx = tx.clone();
                    let max_body = config.crawler.max_body_bytes;
                    let extractor = match &job {
                        Job::Page(page) => crawler.extractors.get(&page.source).cloned(),
                        Job::Robots { .. } => None,
                    };
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
                                let (extractor, rendered) =
                                    extractor.expect("every page belongs to a configured source");
                                let result = fetch::fetch_page(
                                    &clients, &url, max_body, &extractor, rendered,
                                )
                                .await;
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
        // Pages settled without a request (robots.txt blocks) go to the writer too.
        crawler.request_commit()?;
        crawler.deliver_acknowledged(&events)?;

        let too_far = wake.is_some_and(|at| at > Instant::now() + config.max_wait());
        let idle = crawler.in_flight == 0
            && crawler.in_commit.is_empty()
            && (wake.is_none() || too_far || budget.spent());
        if idle {
            // Nothing in flight and nothing to do soon here. Another shard may still hand us
            // a page, so the run ends only when every shard is idle. Marking idle first and
            // checking the inbox after is what makes this sound: a handover sent before its
            // sender went idle is already in our channel when we see the full count.
            if !marked_idle {
                idle_shards.fetch_add(1, Ordering::SeqCst);
                marked_idle = true;
            }
            if idle_shards.load(Ordering::SeqCst) == crawler.routing.shards {
                match inbox.try_recv() {
                    Ok(page) => {
                        idle_shards.fetch_sub(1, Ordering::SeqCst);
                        marked_idle = false;
                        crawler.accept(page, Instant::now());
                        continue;
                    }
                    Err(_) => break,
                }
            }
        } else if marked_idle {
            idle_shards.fetch_sub(1, Ordering::SeqCst);
            marked_idle = false;
        }

        let busy = busy_from.elapsed();
        crawler.stats.coordinator_busy += busy;
        if let Some(counter) = &busy_micros {
            counter.fetch_add(
                busy.as_micros() as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        tokio::select! {
            Some(done) = rx.recv() => {
                let started = Instant::now();
                // Everything that has finished is recorded in one batch and one scheduling
                // pass. Events are delivered only after the writer commits, so a consumer
                // never sees an outcome that a crash could roll back.
                crawler.in_flight -= 1;
                crawler.handle(done).await?;
                let mut batched = 1;
                while batched < MAX_BATCH && let Ok(done) = rx.try_recv() {
                    crawler.in_flight -= 1;
                    crawler.handle(done).await?;
                    batched += 1;
                }
                crawler.request_commit()?;
                crawler.deliver_acknowledged(&events)?;
                let busy = started.elapsed();
                crawler.stats.coordinator_busy += busy;
                if let Some(counter) = &busy_micros {
                    counter.fetch_add(busy.as_micros() as u64, std::sync::atomic::Ordering::Relaxed);
                }
            }
            ack = async { (&mut crawler.in_commit.front_mut().expect("guarded").ack).await }, if !crawler.in_commit.is_empty() => {
                // The oldest batch is committed: deliver its events and hand over its pages.
                let batch = crawler.in_commit.pop_front().expect("guarded");
                ack.map_err(|_| Error::Task("store writer dropped a reply".into()))??;
                crawler.deliver(batch, &events);
                crawler.deliver_acknowledged(&events)?;
            }
            Some(page) = inbox.recv() => {
                // A page another shard found on one of our hosts. It is already in the store.
                let now = Instant::now();
                crawler.accept(page, now);
                while let Ok(page) = inbox.try_recv() {
                    crawler.accept(page, now);
                }
            }
            Some(joined) = tasks.join_next(), if !tasks.is_empty() => {
                if let Err(e) = joined {
                    return Err(Error::Task(e.to_string()));
                }
            }
            _ = sleep_until(wake.unwrap_or_else(Instant::now)), if wake.is_some() => {}
            // Idle but not everyone is: look again soon in case the others finished.
            _ = tokio::time::sleep(Duration::from_millis(20)), if idle => {}
        }
    }

    let complete =
        crawler.frontier.pending() == 0 && crawler.waiting_for_robots.values().all(Vec::is_empty);
    Ok(ShardOutcome {
        stats: crawler.stats,
        complete,
    })
}

impl Crawler<'_> {
    async fn next_step(&mut self, now: Instant) -> Result<Step> {
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
                )
                .await?;
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
                )
                .await?;
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

    async fn handle(&mut self, done: Done) -> Result<()> {
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
                let wait = self.handle_page(&host, page, result, now).await?;
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
                self.writer.clear_robots_failures(self.run, &origin)?;
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
                self.writer
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
        self.writer
            .hold_host(host, unix_now_ms().saturating_add(ms))
    }

    /// Ends a request to `host`: the host may be contacted again after `wait` (or later if
    /// held). The time is stored so a restart cannot shorten it.
    fn release(&mut self, host: &str, now: Instant, wait: Duration) -> Result<()> {
        self.hold(host, now, wait)?;
        self.frontier.release(host, now, Duration::ZERO);
        Ok(())
    }

    /// Records the page outcome and returns how long the host must rest before its next request.
    async fn handle_page(
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
                next,
                activity,
                records,
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
                    records,
                    activity,
                };
                let discovered = self.links_to_follow(&page, links);
                // Continuation pages are part of this page, not links: same depth, no depth
                // limit, and fetched only when this page is new or changed, or its activity
                // fingerprint moved.
                let continuation = next
                    .into_iter()
                    .map(|u| NewPage {
                        url: u.to_string(),
                        source: page.source.clone(),
                        depth: page.depth,
                        hops: 0,
                        continuation: true,
                    })
                    .collect();
                self.commit_with(&page.url, None, outcome, discovered, continuation)
                    .await?;
                Ok(delay)
            }
            PageResult::Invalid {
                status,
                bytes,
                reason: fetch::Invalid::Challenge(detail),
            } => {
                // Treated like a 429 without Retry-After: the host is held, the page retried.
                // Nothing is stored, so the last good content and its hash stay in place.
                self.stats.bytes += bytes as u64;
                let wait = CHALLENGE_HOLD.max(delay);
                self.hold(host, now, wait)?;
                if page.attempts + 1 >= self.config.crawler.max_attempts {
                    let detail = format!("{detail}, gave up after {} attempts", page.attempts + 1);
                    self.commit(
                        &page.url,
                        Some(EventKind::Error),
                        Outcome {
                            status: Some(status),
                            bytes: Some(bytes as i64),
                            detail: Some(detail),
                            ..Outcome::default()
                        },
                        Vec::new(),
                    )
                    .await?;
                } else {
                    self.retry(host, &mut page, now)?;
                }
                Ok(wait)
            }
            PageResult::Invalid {
                status,
                bytes,
                reason,
            } => {
                self.stats.bytes += bytes as u64;
                let (kind, detail) = match reason {
                    fetch::Invalid::Gone(d) => (EventKind::Gone, d),
                    fetch::Invalid::Error(d) | fetch::Invalid::Challenge(d) => {
                        (EventKind::Error, d)
                    }
                };
                self.commit(
                    &page.url,
                    Some(kind),
                    Outcome {
                        status: Some(status),
                        bytes: Some(bytes as i64),
                        detail: Some(detail),
                        ..Outcome::default()
                    },
                    Vec::new(),
                )
                .await?;
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
                            continuation: false,
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
                self.commit(&page.url, Some(kind), outcome, discovered)
                    .await?;
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
                    )
                    .await?;
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
                )
                .await?;
                Ok(delay)
            }
            PageResult::Status(status) if status >= 500 => {
                self.retry_or_fail(host, page, Some(status), format!("HTTP {status}"), now)
                    .await
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
                )
                .await?;
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
                )
                .await?;
                Ok(delay)
            }
            PageResult::Network(reason) => self.retry_or_fail(host, page, None, reason, now).await,
        }
    }

    async fn retry_or_fail(
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
            )
            .await?;
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
        self.writer.record_attempt(&page.url, page.attempts)?;
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
                continuation: false,
            })
            .collect()
    }

    /// Commits the outcome, its event and the pages it led to atomically, then queues the
    /// pages that are new to this run and reports the event.
    async fn commit(
        &mut self,
        url: &str,
        kind: Option<EventKind>,
        outcome: Outcome,
        discovered: Vec<NewPage>,
    ) -> Result<()> {
        self.commit_with(url, kind, outcome, discovered, Vec::new())
            .await
    }

    async fn commit_with(
        &mut self,
        url: &str,
        kind: Option<EventKind>,
        outcome: Outcome,
        discovered: Vec<NewPage>,
        continuation: Vec<NewPage>,
    ) -> Result<()> {
        let (event, added) = self
            .writer
            .complete(self.run, url, kind, outcome, discovered, continuation)
            .await?;
        let now = Instant::now();
        for p in added {
            let Ok(parsed) = Url::parse(&p.url) else {
                continue;
            };
            let host = urlnorm::host(&parsed);
            if self.routing.mine(&host) {
                self.frontier.push(&host, Work::Page(to_page(p)), now);
            } else {
                // Another shard's host: sent after this batch is committed, so the receiving
                // shard never sees a page the store does not know yet.
                self.outbound.push(p);
            }
        }
        self.stats.events += 1;
        if matches!(event.kind, EventKind::Changed) {
            self.stats.changed += 1;
        }
        self.pending_events.push(event);
        Ok(())
    }

    /// Sends a commit request for everything this shard recorded since the last one. The
    /// batch is delivered (events to the consumer, cross-shard pages to their owners) when
    /// the writer acknowledges it; meanwhile the shard keeps scheduling.
    fn request_commit(&mut self) -> Result<()> {
        if self.pending_events.is_empty() && self.outbound.is_empty() {
            return Ok(());
        }
        let ack = self.writer.request_flush()?;
        self.in_commit.push_back(InCommit {
            ack,
            events: std::mem::take(&mut self.pending_events),
            outbound: std::mem::take(&mut self.outbound),
        });
        Ok(())
    }

    /// Delivers every batch whose commit has already been acknowledged, oldest first, without
    /// waiting. Called on every pass so acknowledgements never queue up behind new fetches.
    fn deliver_acknowledged(&mut self, events: &mpsc::UnboundedSender<Event>) -> Result<()> {
        while let Some(front) = self.in_commit.front_mut() {
            match front.ack.try_recv() {
                Ok(result) => {
                    result?;
                    let batch = self.in_commit.pop_front().expect("front exists");
                    self.deliver(batch, events);
                }
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => break,
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    return Err(Error::Task("store writer dropped a reply".into()));
                }
            }
        }
        Ok(())
    }

    fn deliver(&mut self, batch: InCommit, events: &mpsc::UnboundedSender<Event>) {
        for event in batch.events {
            let _ = events.send(event);
        }
        for p in batch.outbound {
            let Ok(parsed) = Url::parse(&p.url) else {
                continue;
            };
            let owner = self.routing.owner(&urlnorm::host(&parsed));
            self.stats.handed_over += 1;
            // A closed inbox means that shard has finished; its hosts' pages stay pending in
            // the store and the next run picks them up.
            let _ = self.routing.handover[owner].send(p);
        }
    }

    /// Queues a page another shard handed over.
    fn accept(&mut self, p: NewPage, now: Instant) {
        let Ok(parsed) = Url::parse(&p.url) else {
            return;
        };
        self.frontier
            .push(&urlnorm::host(&parsed), Work::Page(to_page(p)), now);
    }
}

fn to_page(p: NewPage) -> Page {
    Page {
        url: p.url,
        source: p.source,
        depth: p.depth,
        hops: p.hops,
        attempts: 0,
    }
}

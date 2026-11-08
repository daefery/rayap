//! End-to-end crawls against a local HTTP server that records every request.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use rayap_core::{Config, Event, EventKind, Options, Store, crawl};

/// What the server returns for a path. Pages can be changed between crawls.
#[derive(Clone)]
enum Reply {
    Html(String),
    Text(String),
    Status(u16),
    Redirect(String),
    TooManyRequests {
        retry_after: &'static str,
        times: u32,
    },
    /// Return `status` for the first `times` requests, then behave like `then`.
    FailThen {
        status: u16,
        times: u32,
        then: Box<Reply>,
    },
}

#[derive(Clone, Default)]
struct Site {
    routes: Arc<Mutex<HashMap<String, Reply>>>,
    log: Arc<Mutex<Vec<(Instant, String, String)>>>,
}

impl Site {
    fn set(&self, path: &str, reply: Reply) {
        self.routes.lock().unwrap().insert(path.to_string(), reply);
    }

    fn requests(&self) -> Vec<String> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .map(|(_, p, _)| p.clone())
            .collect()
    }

    fn user_agents(&self) -> Vec<String> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .map(|(_, _, ua)| ua.clone())
            .collect()
    }

    fn times(&self) -> Vec<Instant> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .map(|(t, _, _)| *t)
            .collect()
    }
}

async fn serve(State(site): State<Site>, headers: HeaderMap, request: Request) -> Response {
    let path = request
        .uri()
        .path_and_query()
        .map(|p| p.to_string())
        .unwrap_or_default();
    let ua = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    site.log
        .lock()
        .unwrap()
        .push((Instant::now(), path.clone(), ua));
    let reply = site.routes.lock().unwrap().get(&path).cloned();
    match reply {
        None => StatusCode::NOT_FOUND.into_response(),
        Some(Reply::Html(body)) => {
            ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response()
        }
        Some(Reply::Text(body)) => ([(header::CONTENT_TYPE, "text/plain")], body).into_response(),
        Some(Reply::Status(code)) => StatusCode::from_u16(code).unwrap().into_response(),
        Some(Reply::Redirect(to)) => {
            (StatusCode::MOVED_PERMANENTLY, [(header::LOCATION, to)]).into_response()
        }
        Some(Reply::FailThen {
            status,
            times,
            then,
        }) => {
            if times <= 1 {
                site.set(&path, *then);
            } else {
                site.set(
                    &path,
                    Reply::FailThen {
                        status,
                        times: times - 1,
                        then,
                    },
                );
            }
            StatusCode::from_u16(status).unwrap().into_response()
        }
        Some(Reply::TooManyRequests { retry_after, times }) => {
            if times == 0 {
                return (
                    [(header::CONTENT_TYPE, "text/html")],
                    "<p>ok after waiting</p>",
                )
                    .into_response();
            }
            site.set(
                &path,
                Reply::TooManyRequests {
                    retry_after,
                    times: times - 1,
                },
            );
            (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, retry_after)],
            )
                .into_response()
        }
    }
}

async fn start(site: Site) -> SocketAddr {
    let app = Router::new().fallback(serve).with_state(site);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

fn config(addr: SocketAddr, delay_ms: u64, depth: u32) -> Config {
    Config::from_toml(&format!(
        r#"
        [crawler]
        contact = "mailto:test@example.org"
        per_host_delay_ms = {delay_ms}
        request_timeout_ms = 5000
        max_attempts = 3
        retry_backoff_ms = 50
        [[source]]
        name = "grants"
        url = "http://{addr}/"
        max_depth = {depth}
        "#
    ))
    .unwrap()
}

/// `RAYAP_TEST_SHARDS=N` runs every test with N coordinators; the behaviour must not differ.
fn shards() -> usize {
    std::env::var("RAYAP_TEST_SHARDS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1)
}

async fn run(
    config: &Config,
    store: &mut Store,
    mut options: Options,
) -> (rayap_core::Stats, Vec<Event>) {
    if options.shards == 0 {
        options.shards = shards();
    }
    let mut events = Vec::new();
    let stats = crawl(config, store, options, |e| events.push(e.clone()))
        .await
        .unwrap();
    (stats, events)
}

fn kinds(events: &[Event]) -> Vec<(String, EventKind)> {
    let mut v: Vec<_> = events
        .iter()
        .map(|e| (url::Url::parse(&e.url).unwrap().path().to_string(), e.kind))
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

#[tokio::test]
async fn crawls_follows_links_and_respects_robots() {
    let site = Site::default();
    site.set(
        "/robots.txt",
        Reply::Text("User-agent: *\nDisallow: /private\n".into()),
    );
    site.set(
        "/",
        Reply::Html(r#"<title>Grants</title><a href="/call-1">1</a><a href="/private/x">p</a><a href="https://elsewhere.example/">x</a>"#.into()),
    );
    site.set(
        "/call-1",
        Reply::Html("<p>Deadline 1 Dec</p><a href='/call-2'>deeper</a>".into()),
    );
    let addr = start(site.clone()).await;

    let mut store = Store::open_in_memory().unwrap();
    let (stats, events) = run(&config(addr, 0, 1), &mut store, Options::default()).await;

    assert!(stats.finished);
    assert_eq!(
        kinds(&events),
        [
            ("/".into(), EventKind::New),
            ("/call-1".into(), EventKind::New),
            ("/private/x".into(), EventKind::Blocked)
        ]
    );
    let requests = site.requests();
    assert_eq!(
        requests[0], "/robots.txt",
        "robots.txt is fetched before any page"
    );
    assert!(
        !requests.iter().any(|p| p.starts_with("/private")),
        "disallowed path was requested"
    );
    assert!(
        !requests.contains(&"/call-2".to_string()),
        "max_depth = 1 must stop at call-1"
    );
    assert!(
        site.user_agents()
            .iter()
            .all(|ua| ua.contains("rayap/") && ua.contains("mailto:test@example.org"))
    );
    assert_eq!(
        events
            .iter()
            .find(|e| e.url.ends_with('/'))
            .unwrap()
            .title
            .as_deref(),
        Some("Grants")
    );
}

#[tokio::test]
async fn second_run_reports_only_real_changes() {
    let site = Site::default();
    site.set(
        "/",
        Reply::Html("<a href='/a'>a</a><a href='/b'>b</a><script>t=1</script>".into()),
    );
    site.set("/a", Reply::Html("<p>Deadline: 1 Dec 2026</p>".into()));
    site.set("/b", Reply::Html("<p>Open call</p>".into()));
    let addr = start(site.clone()).await;
    let cfg = config(addr, 0, 1);
    let mut store = Store::open_in_memory().unwrap();
    run(&cfg, &mut store, Options::default()).await;

    site.set(
        "/",
        Reply::Html("<a href='/a'>a</a><a href='/b'>b</a><script>t=2</script>".into()),
    );
    site.set("/a", Reply::Html("<p>Deadline: 15 Dec 2026</p>".into()));
    let (stats, events) = run(&cfg, &mut store, Options::default()).await;

    assert_eq!(
        kinds(&events),
        [
            ("/".into(), EventKind::Unchanged),
            ("/a".into(), EventKind::Changed),
            ("/b".into(), EventKind::Unchanged)
        ]
    );
    assert_eq!(stats.changed, 1);
    let changed = events
        .iter()
        .find(|e| e.kind == EventKind::Changed)
        .unwrap();
    assert!(changed.prev_hash.is_some() && changed.prev_hash != changed.hash);
}

#[tokio::test]
async fn redirect_target_is_crawled_as_its_own_page() {
    let site = Site::default();
    site.set("/", Reply::Redirect("/grants/2026".into()));
    site.set("/grants/2026", Reply::Html("<p>Calls</p>".into()));
    let addr = start(site.clone()).await;
    let mut store = Store::open_in_memory().unwrap();
    let (_, events) = run(&config(addr, 0, 0), &mut store, Options::default()).await;

    assert_eq!(
        kinds(&events),
        [
            ("/".into(), EventKind::Redirect),
            ("/grants/2026".into(), EventKind::New)
        ]
    );
    assert!(
        events[0]
            .detail
            .as_deref()
            .unwrap()
            .ends_with("/grants/2026")
    );
}

#[tokio::test]
async fn gone_and_server_errors() {
    let site = Site::default();
    site.set(
        "/",
        Reply::Html("<a href='/old'>o</a><a href='/broken'>b</a>".into()),
    );
    site.set("/old", Reply::Status(410));
    site.set("/broken", Reply::Status(500));
    let addr = start(site.clone()).await;
    let mut cfg = config(addr, 0, 1);
    cfg.crawler.max_attempts = 2;
    let mut store = Store::open_in_memory().unwrap();
    let (_, events) = run(&cfg, &mut store, Options::default()).await;

    assert_eq!(
        kinds(&events),
        [
            ("/".into(), EventKind::New),
            ("/broken".into(), EventKind::Error),
            ("/old".into(), EventKind::Gone)
        ]
    );
    assert_eq!(
        site.requests().iter().filter(|p| *p == "/broken").count(),
        2,
        "retried up to max_attempts"
    );
}

#[tokio::test]
async fn honours_retry_after() {
    let site = Site::default();
    site.set(
        "/",
        Reply::TooManyRequests {
            retry_after: "1",
            times: 1,
        },
    );
    let addr = start(site.clone()).await;
    let mut store = Store::open_in_memory().unwrap();
    let (_, events) = run(&config(addr, 0, 0), &mut store, Options::default()).await;

    assert_eq!(kinds(&events), [("/".into(), EventKind::New)]);
    let times = site.times();
    let page_hits: Vec<_> = site
        .requests()
        .iter()
        .zip(&times)
        .filter(|(p, _)| *p == "/")
        .map(|(_, t)| *t)
        .collect();
    assert_eq!(page_hits.len(), 2);
    assert!(
        page_hits[1] - page_hits[0] >= std::time::Duration::from_millis(1000),
        "retried before Retry-After"
    );
}

#[tokio::test]
async fn unreachable_robots_blocks_the_host() {
    let site = Site::default();
    site.set("/robots.txt", Reply::Status(503));
    site.set("/", Reply::Html("<p>x</p>".into()));
    let addr = start(site.clone()).await;
    let mut store = Store::open_in_memory().unwrap();
    let (_, events) = run(&config(addr, 0, 0), &mut store, Options::default()).await;

    assert_eq!(kinds(&events), [("/".into(), EventKind::Blocked)]);
    assert_eq!(
        site.requests(),
        ["/robots.txt", "/robots.txt", "/robots.txt"],
        "robots.txt is retried up to max_attempts and no page is fetched meanwhile"
    );
}

#[tokio::test]
async fn per_host_delay_is_respected() {
    let site = Site::default();
    site.set(
        "/",
        Reply::Html("<a href='/1'>1</a><a href='/2'>2</a>".into()),
    );
    site.set("/1", Reply::Html("one".into()));
    site.set("/2", Reply::Html("two".into()));
    let addr = start(site.clone()).await;
    let mut store = Store::open_in_memory().unwrap();
    run(&config(addr, 200, 1), &mut store, Options::default()).await;

    let times = site.times();
    assert_eq!(times.len(), 4, "robots.txt + 3 pages");
    for pair in times.windows(2) {
        assert!(
            pair[1] - pair[0] >= std::time::Duration::from_millis(200),
            "two requests closer than the delay"
        );
    }
}

#[tokio::test]
async fn interrupted_run_resumes_without_refetching_finished_pages() {
    let site = Site::default();
    site.set(
        "/",
        Reply::Html("<a href='/1'>1</a><a href='/2'>2</a><a href='/3'>3</a>".into()),
    );
    for p in ["/1", "/2", "/3"] {
        site.set(p, Reply::Html(format!("<p>{p}</p>")));
    }
    let addr = start(site.clone()).await;
    let cfg = config(addr, 0, 1);
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");

    let first = {
        let mut store = Store::open(&db).unwrap();
        let (stats, events) = run(
            &cfg,
            &mut store,
            Options {
                max_fetches: Some(2),
                ..Options::default()
            },
        )
        .await;
        assert!(!stats.finished);
        events
    };
    assert_eq!(first.len(), 2);

    let mut store = Store::open(&db).unwrap();
    let (stats, second) = run(&cfg, &mut store, Options::default()).await;
    assert!(stats.resumed && stats.finished);
    assert_eq!(second.len(), 2, "only the two remaining pages are fetched");
    let page_requests: Vec<_> = site
        .requests()
        .into_iter()
        .filter(|p| p != "/robots.txt")
        .collect();
    assert_eq!(
        page_requests.len(),
        4,
        "every page fetched exactly once across both processes"
    );
    assert_eq!(store.events_after(0, 100).unwrap().len(), 4);
}

#[tokio::test]
async fn transient_robots_failure_is_retried_not_treated_as_a_block() {
    let site = Site::default();
    site.set(
        "/robots.txt",
        Reply::FailThen {
            status: 503,
            times: 1,
            then: Box::new(Reply::Text("User-agent: *\nDisallow: /no\n".into())),
        },
    );
    site.set("/", Reply::Html("<p>ok</p>".into()));
    let addr = start(site.clone()).await;
    let mut store = Store::open_in_memory().unwrap();
    let (_, events) = run(&config(addr, 0, 0), &mut store, Options::default()).await;
    assert_eq!(kinds(&events), [("/".into(), EventKind::New)]);
    assert_eq!(site.requests(), ["/robots.txt", "/robots.txt", "/"]);
}

#[tokio::test]
async fn robots_redirect_hops_are_spaced_by_the_host_delay() {
    let site = Site::default();
    site.set("/robots.txt", Reply::Redirect("/r1".into()));
    site.set("/r1", Reply::Redirect("/r2".into()));
    site.set(
        "/r2",
        Reply::Text("User-agent: *\nDisallow: /private\n".into()),
    );
    site.set(
        "/",
        Reply::Html("<a href='/private/x'>p</a><a href='/ok'>o</a>".into()),
    );
    site.set("/ok", Reply::Html("ok".into()));
    let addr = start(site.clone()).await;
    let mut store = Store::open_in_memory().unwrap();
    let (_, events) = run(&config(addr, 150, 1), &mut store, Options::default()).await;

    assert_eq!(site.requests()[..3], ["/robots.txt", "/r1", "/r2"]);
    assert!(
        kinds(&events).contains(&("/private/x".into(), EventKind::Blocked)),
        "rules from the redirected file apply"
    );
    for pair in site.times().windows(2) {
        assert!(
            pair[1] - pair[0] >= std::time::Duration::from_millis(150),
            "requests closer than the host delay"
        );
    }
}

#[tokio::test]
async fn robots_retry_after_holds_the_host() {
    let site = Site::default();
    site.set(
        "/robots.txt",
        Reply::TooManyRequests {
            retry_after: "1",
            times: 1,
        },
    );
    site.set("/", Reply::Html("<p>x</p>".into()));
    let addr = start(site.clone()).await;
    let mut store = Store::open_in_memory().unwrap();
    run(&config(addr, 0, 0), &mut store, Options::default()).await;
    let log: Vec<_> = site.requests().into_iter().zip(site.times()).collect();
    let first = log[0].1;
    assert!(
        log.iter()
            .skip(1)
            .all(|(_, t)| *t - first >= std::time::Duration::from_millis(1000)),
        "host contacted again before Retry-After: {:?}",
        log.iter()
            .map(|(p, t)| (p.clone(), *t - first))
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn retry_after_survives_a_restart() {
    let site = Site::default();
    site.set(
        "/",
        Reply::Html("<a href='/a'>a</a><a href='/b'>b</a>".into()),
    );
    site.set(
        "/a",
        Reply::TooManyRequests {
            retry_after: "2",
            times: 1,
        },
    );
    site.set("/b", Reply::Html("b".into()));
    let addr = start(site.clone()).await;
    let cfg = config(addr, 0, 1);
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    {
        let mut store = Store::open(&db).unwrap();
        // "/" then "/a" (429): stop right after the 429.
        let (stats, _) = run(
            &cfg,
            &mut store,
            Options {
                max_fetches: Some(2),
                ..Options::default()
            },
        )
        .await;
        assert!(!stats.finished);
    }
    let throttled_at = *site.times().last().unwrap();
    let mut store = Store::open(&db).unwrap();
    run(&cfg, &mut store, Options::default()).await;
    let after: Vec<_> = site
        .requests()
        .into_iter()
        .zip(site.times())
        .filter(|(_, t)| *t > throttled_at)
        .collect();
    assert!(!after.is_empty());
    for (path, t) in after {
        assert!(
            t - throttled_at >= std::time::Duration::from_millis(1900),
            "{path} requested before Retry-After after restart"
        );
    }
}

#[tokio::test]
async fn redirect_to_another_host_does_not_widen_the_crawl() {
    let site = Site::default();
    let other = Site::default();
    let other_addr = start(other.clone()).await;
    other.set("/", Reply::Html("<a href='/deep'>d</a>".into()));
    other.set("/deep", Reply::Html("deep".into()));
    // A different host name for the same machine, so the two sites are different hosts.
    site.set(
        "/",
        Reply::Redirect(format!("http://localhost:{}/", other_addr.port())),
    );
    let addr = start(site.clone()).await;
    let mut store = Store::open_in_memory().unwrap();
    run(&config(addr, 0, 2), &mut store, Options::default()).await;
    assert!(
        !other.requests().contains(&"/deep".to_string()),
        "links on the redirect target's host were followed"
    );
    assert!(
        other.requests().contains(&"/".to_string()),
        "the redirect target itself is crawled"
    );
}

#[tokio::test]
async fn endless_redirect_chains_stop() {
    let site = Site::default();
    for i in 0..30 {
        let path = if i == 0 {
            "/".to_string()
        } else {
            format!("/r{i}")
        };
        site.set(&path, Reply::Redirect(format!("/r{}", i + 1)));
    }
    let addr = start(site.clone()).await;
    let mut store = Store::open_in_memory().unwrap();
    let (stats, events) = run(&config(addr, 0, 0), &mut store, Options::default()).await;
    assert!(stats.finished);
    assert_eq!(events.last().unwrap().kind, EventKind::Error);
    assert!(site.requests().len() <= 12, "robots.txt + at most 11 hops");
}

#[tokio::test]
async fn huge_retry_after_ends_the_run_without_panicking() {
    let site = Site::default();
    site.set(
        "/",
        Reply::TooManyRequests {
            retry_after: "18446744073709551615",
            times: 5,
        },
    );
    let addr = start(site.clone()).await;
    let mut store = Store::open_in_memory().unwrap();
    let (stats, events) = run(&config(addr, 0, 0), &mut store, Options::default()).await;
    assert!(!stats.finished, "the page stays pending for a later run");
    assert!(events.is_empty());
    assert_eq!(site.requests(), ["/robots.txt", "/"]);
}

#[tokio::test]
async fn cross_host_robots_redirect_applies_crawl_delay_to_the_origin() {
    let site = Site::default();
    let other = Site::default();
    let other_addr = start(other.clone()).await;
    other.set(
        "/rules.txt",
        Reply::Text("User-agent: *\nCrawl-delay: 1\n".into()),
    );
    site.set(
        "/robots.txt",
        Reply::Redirect(format!("http://localhost:{}/rules.txt", other_addr.port())),
    );
    site.set("/", Reply::Html("<p>x</p>".into()));
    let addr = start(site.clone()).await;
    let mut store = Store::open_in_memory().unwrap();
    run(&config(addr, 0, 0), &mut store, Options::default()).await;
    let times = site.times();
    assert_eq!(site.requests(), ["/robots.txt", "/"]);
    let rules_at = other.times()[0];
    assert!(
        times[1] - rules_at >= std::time::Duration::from_millis(1000),
        "page fetched before the origin's Crawl-delay"
    );
}

/// A Retry-After beyond max_wait ends each run after one robots.txt attempt; the failures
/// must still add up across resumed runs.
#[tokio::test]
async fn robots_failures_add_up_across_resumed_runs() {
    let site = Site::default();
    site.set("/robots.txt", Reply::Status(503));
    site.set("/", Reply::Html("<p>x</p>".into()));
    let addr = start(site.clone()).await;
    let mut cfg = config(addr, 0, 0);
    // Each failure holds the host for 400 ms, longer than max_wait, so every run ends after
    // one attempt and the next run (after the hold) makes the next attempt.
    cfg.crawler.retry_backoff_ms = 400;
    cfg.crawler.max_wait_ms = 0;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let mut all = Vec::new();
    for _ in 0..6 {
        let mut store = Store::open(&db).unwrap();
        let (stats, events) = run(&cfg, &mut store, Options::default()).await;
        all.extend(events);
        if stats.finished {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1300)).await;
    }
    assert_eq!(kinds(&all), [("/".into(), EventKind::Blocked)]);
    let robots = site
        .requests()
        .iter()
        .filter(|p| *p == "/robots.txt")
        .count();
    assert_eq!(robots, 3, "max_attempts = 3 across runs");
}

#[tokio::test]
async fn panicking_event_callback_still_leaves_the_host_delay_stored() {
    let site = Site::default();
    site.set("/", Reply::Html("<a href='/next'>n</a>".into()));
    site.set("/next", Reply::Html("n".into()));
    let addr = start(site.clone()).await;
    let cfg = config(addr, 2_000, 1);
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    {
        let cfg = cfg.clone();
        let db = db.clone();
        let crashed = tokio::spawn(async move {
            let mut store = Store::open(&db).unwrap();
            crawl(
                &cfg,
                &mut store,
                Options {
                    shards: shards(),
                    ..Options::default()
                },
                |_| panic!("consumer crashed"),
            )
            .await
        })
        .await;
        assert!(crashed.is_err(), "the callback panic propagates");
    }
    let crashed_at = *site.times().last().unwrap();
    // Deterministic check: the host's delay is in the store even though the consumer crashed.
    let hold_ms = Store::open(&db)
        .unwrap()
        .active_holds()
        .unwrap()
        .into_iter()
        .find(|(h, _)| h == "127.0.0.1")
        .map(|(_, until)| until - rayap_core::store::unix_now_ms())
        .expect("no stored hold for the host after the crash");
    assert!(hold_ms > 1500, "stored hold only {hold_ms} ms");
    let mut store = Store::open(&db).unwrap();
    run(&cfg, &mut store, Options::default()).await;
    let first_after = site.times().into_iter().find(|t| *t > crashed_at).unwrap();
    assert!(
        first_after - crashed_at >= std::time::Duration::from_millis(1900),
        "host contacted {:?} after the crash, before its 2 s delay",
        first_after - crashed_at
    );
}

#[tokio::test]
async fn long_retry_after_is_stored_in_full() {
    let site = Site::default();
    site.set(
        "/",
        Reply::TooManyRequests {
            retry_after: "3456000",
            times: 1,
        },
    ); // 40 days
    let addr = start(site.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("state.db");
    let mut store = Store::open(&db).unwrap();
    run(&config(addr, 0, 0), &mut store, Options::default()).await;
    let holds = store.active_holds().unwrap();
    let host_hold = holds.iter().find(|(h, _)| h == "127.0.0.1").unwrap().1;
    let days = (host_hold - rayap_core::store::unix_now_ms()) as f64 / 86_400_000.0;
    assert!(days > 39.9, "stored hold is {days:.1} days");
}

fn tiktok_config(addr: SocketAddr, max_comments: u32) -> Config {
    Config::from_toml(&format!(
        r#"
        [crawler]
        contact = "mailto:test@example.org"
        per_host_delay_ms = 300
        request_timeout_ms = 5000
        max_attempts = 2
        retry_backoff_ms = 50
        [[source]]
        name = "frogs"
        url = "http://{addr}/@tiktok/video/7106594312292453675"
        kind = "tiktok"
        max_comments = {max_comments}
        [[source]]
        name = "deleted"
        url = "http://{addr}/@x/video/1"
        kind = "tiktok"
        "#
    ))
    .unwrap()
}

#[tokio::test]
async fn tiktok_post_and_comments_become_records() {
    let site = Site::default();
    site.set(
        "/robots.txt",
        Reply::Text("User-agent: *\nDisallow: /inapp\n".into()),
    );
    site.set(
        "/@tiktok/video/7106594312292453675",
        Reply::Html(include_str!("fixtures/tiktok/post.html").into()),
    );
    site.set(
        "/api/comment/list/?aid=1988&aweme_id=7106594312292453675&count=20&cursor=0",
        Reply::Text(include_str!("fixtures/tiktok/comments.json").into()),
    );
    site.set(
        "/api/comment/list/?aid=1988&aweme_id=7106594312292453675&count=20&cursor=20",
        Reply::Text(include_str!("fixtures/tiktok/comments-last.json").into()),
    );
    site.set(
        "/@x/video/1",
        Reply::Html(include_str!("fixtures/tiktok/post-gone.html").into()),
    );
    let addr = start(site.clone()).await;
    let cfg = tiktok_config(addr, 40);
    let mut store = Store::open_in_memory().unwrap();

    let (stats, events) = run(&cfg, &mut store, Options::default()).await;
    assert!(stats.finished);
    assert_eq!(
        kinds(&events),
        [
            (
                "/@tiktok/video/7106594312292453675".to_string(),
                EventKind::New
            ),
            ("/@x/video/1".to_string(), EventKind::Gone),
            ("/api/comment/list/".to_string(), EventKind::New),
            ("/api/comment/list/".to_string(), EventKind::New),
        ]
    );
    let post_event = events
        .iter()
        .find(|e| e.url.contains("/video/7106"))
        .unwrap();
    assert_eq!(
        (post_event.records, post_event.bytes.is_some()),
        (Some(1), true)
    );
    let gone = events
        .iter()
        .find(|e| e.url.ends_with("/@x/video/1"))
        .unwrap();
    assert_eq!(
        (gone.status, gone.detail.as_deref(), gone.records),
        (Some(200), Some("item doesn't exist"), None),
        "a 200 for a deleted post is gone, not content"
    );

    let posts = store.records_after(0, Some("post"), 100).unwrap();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].data["author"]["handle"], "tiktok");
    assert_eq!(posts[0].source, "frogs");
    let comments = store.records_after(0, Some("comment"), 100).unwrap();
    assert_eq!(comments.len(), 4, "3 on the first page + 1 on the last");
    assert_eq!(
        comments
            .iter()
            .map(|c| c.data["page_cursor"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [0, 0, 0, 20]
    );
    assert!(
        comments
            .iter()
            .all(|c| c.data["post_id"] == "7106594312292453675")
    );

    // Politeness: robots.txt first, then every request on the host spaced by the delay.
    let requests = site.requests();
    assert_eq!(requests[0], "/robots.txt");
    assert_eq!(requests.len(), 5, "{requests:?}");
    let times = site.times();
    for pair in times.windows(2) {
        assert!(
            pair[1] - pair[0] >= std::time::Duration::from_millis(290),
            "{requests:?}"
        );
    }
    assert!(site.user_agents().iter().all(|ua| ua.contains("rayap/")));

    // Second run: nothing changed, so no record is written at all, let alone duplicated.
    let (_, events) = run(&cfg, &mut store, Options::default()).await;
    let unchanged: Vec<&Event> = events
        .iter()
        .filter(|e| e.kind == EventKind::Unchanged)
        .collect();
    assert_eq!(unchanged.len(), 3, "{:?}", kinds(&events));
    assert!(
        unchanged.iter().all(|e| e.records == Some(0)),
        "{unchanged:?}"
    );
    assert_eq!(store.records_after(0, None, 100).unwrap().len(), 5);
    assert!(
        store
            .records_after(0, None, 100)
            .unwrap()
            .iter()
            .all(|r| r.run == 1),
        "records still carry the run that produced them"
    );
}

#[tokio::test]
async fn tiktok_comment_cap_and_shell_pages() {
    let site = Site::default();
    site.set(
        "/@tiktok/video/7106594312292453675",
        Reply::Html(include_str!("fixtures/tiktok/post.html").into()),
    );
    site.set(
        "/api/comment/list/?aid=1988&aweme_id=7106594312292453675&count=20&cursor=0",
        Reply::Text(include_str!("fixtures/tiktok/comments.json").into()),
    );
    site.set(
        "/@x/video/1",
        Reply::Html("<html><body><div id='app'></div></body></html>".into()),
    );
    let addr = start(site.clone()).await;
    let cfg = tiktok_config(addr, 20);
    let mut store = Store::open_in_memory().unwrap();
    let (_, events) = run(&cfg, &mut store, Options::default()).await;
    assert!(
        !site.requests().iter().any(|p| p.contains("cursor=20")),
        "max_comments = 20 stops after one page: {:?}",
        site.requests()
    );
    let shell = events
        .iter()
        .find(|e| e.url.ends_with("/@x/video/1"))
        .unwrap();
    assert_eq!(shell.kind, EventKind::Error);
    assert!(
        shell.detail.as_deref().unwrap().contains("no embedded"),
        "{shell:?}"
    );
    assert!(
        shell.hash.is_none(),
        "an empty shell is never fingerprinted as content"
    );
}

#[tokio::test]
async fn planned_runs_skip_pages_not_due_and_refetch_comments_only_on_change() {
    use rayap_core::Plan;
    use rayap_core::revisit::Prior;

    let site = Site::default();
    let post = include_str!("fixtures/tiktok/post.html");
    site.set(
        "/@tiktok/video/7106594312292453675",
        Reply::Html(post.into()),
    );
    site.set(
        "/api/comment/list/?aid=1988&aweme_id=7106594312292453675&count=20&cursor=0",
        Reply::Text(include_str!("fixtures/tiktok/comments.json").into()),
    );
    site.set(
        "/@x/video/1",
        Reply::Html(include_str!("fixtures/tiktok/post-gone.html").into()),
    );
    let addr = start(site.clone()).await;
    let cfg = tiktok_config(addr, 20);
    let plan = Plan {
        budget: 1,
        max_interval_days: 30.0,
        prior: Prior {
            rate: 0.2,
            weight: 0.5,
        },
    };
    let planned = || Options {
        plan: Some(plan.clone()),
        ..Options::default()
    };
    let mut store = Store::open_in_memory().unwrap();

    // Run 1, planned: both seeds are unknown, so both are fetched regardless of the budget.
    let (stats, events) = run(&cfg, &mut store, planned()).await;
    assert_eq!(
        (stats.deferred, events.len()),
        (0, 3),
        "{:?}",
        kinds(&events)
    );

    // Run 2, planned, budget 1: one of the two known pages is deferred. The visited post is
    // unchanged, so its comment page is not fetched again.
    let before = site.requests().len();
    let (stats, events) = run(&cfg, &mut store, planned()).await;
    assert_eq!(stats.deferred, 1);
    assert_eq!(events.len(), 1, "{:?}", kinds(&events));
    let new_requests: Vec<String> = site.requests()[before..]
        .iter()
        .filter(|p| !p.ends_with("/robots.txt"))
        .cloned()
        .collect();
    assert_eq!(
        new_requests.len(),
        1,
        "one page request besides robots.txt: {new_requests:?}"
    );
    assert!(matches!(
        events[0].kind,
        EventKind::Unchanged | EventKind::Gone
    ));

    // A new comment arrives: the post text is unchanged, but its comment count moved, and
    // that alone refetches the comment page.
    site.set(
        "/@tiktok/video/7106594312292453675",
        Reply::Html(post.replace("\"commentCount\": 1274", "\"commentCount\": 1275")),
    );
    let before = site.requests().len();
    let (_, events) = run(
        &cfg,
        &mut store,
        Options {
            plan: Some(Plan {
                budget: 2,
                ..plan.clone()
            }),
            ..Options::default()
        },
    )
    .await;
    let post_event = events
        .iter()
        .find(|e| e.url.contains("/video/7106"))
        .unwrap();
    assert_eq!(
        post_event.kind,
        EventKind::Unchanged,
        "{:?}",
        kinds(&events)
    );
    assert!(
        site.requests()[before..]
            .iter()
            .any(|p| p.contains("/api/comment/list/")),
        "comments refetched on activity: {:?}",
        &site.requests()[before..]
    );

    // Edit the post, then give the budget to both pages: the post is changed, and its
    // comment page is refetched for that reason too.
    site.set(
        "/@tiktok/video/7106594312292453675",
        Reply::Html(post.replace("how many frogs", "how many toads")),
    );
    let before = site.requests().len();
    let (_, events) = run(
        &cfg,
        &mut store,
        Options {
            plan: Some(Plan {
                budget: 2,
                ..plan.clone()
            }),
            ..Options::default()
        },
    )
    .await;
    let post_event = events
        .iter()
        .find(|e| e.url.contains("/video/7106"))
        .unwrap();
    assert_eq!(post_event.kind, EventKind::Changed);
    let new_requests = &site.requests()[before..];
    assert!(
        new_requests
            .iter()
            .any(|p| p.contains("/api/comment/list/")),
        "comments refetched after a change: {new_requests:?}"
    );
    assert_eq!(
        store.records_after(0, Some("comment"), 100).unwrap().len(),
        3,
        "replaced, not duplicated"
    );

    // The planner has seen three revisits of the post (two unchanged, one changed) and reports a rate.
    let rates = store.rates("frogs", plan.prior).unwrap();
    assert_eq!(
        rates.len(),
        1,
        "continuation pages are not planned: {rates:?}"
    );
    assert!(rates[0].1 > 0.0);
}

#[tokio::test]
async fn challenge_page_holds_the_host_and_retries_without_storing() {
    let challenge = "<!DOCTYPE html><html lang=\"en\"><head><link rel=\"preload\" as=\"script\" \
        href=\"https://x.test/webmssdk/1.0.0.417/webmssdk.js\"><title data-rh=\"true\"></title></head>\
        <body><div id=\"captcha_container\"></div></body></html>";
    let site = Site::default();
    site.set(
        "/@tiktok/video/7106594312292453675",
        Reply::Html(include_str!("fixtures/tiktok/post.html").into()),
    );
    site.set(
        "/api/comment/list/?aid=1988&aweme_id=7106594312292453675&count=20&cursor=0",
        Reply::Text(include_str!("fixtures/tiktok/comments.json").into()),
    );
    site.set("/@x/video/1", Reply::Html(challenge.into()));
    let addr = start(site.clone()).await;
    // max_wait short, so the run ends instead of waiting out the 5-minute challenge hold.
    let cfg = Config::from_toml(&format!(
        r#"
        [crawler]
        contact = "mailto:test@example.org"
        per_host_delay_ms = 100
        request_timeout_ms = 5000
        max_attempts = 3
        retry_backoff_ms = 50
        max_wait_ms = 1000
        [[source]]
        name = "frogs"
        url = "http://{addr}/@tiktok/video/7106594312292453675"
        kind = "tiktok"
        max_comments = 20
        [[source]]
        name = "challenged"
        url = "http://{addr}/@x/video/1"
        kind = "tiktok"
        "#
    ))
    .unwrap();
    let mut store = Store::open_in_memory().unwrap();
    let (stats, events) = run(&cfg, &mut store, Options::default()).await;
    assert!(
        !stats.finished,
        "the challenged page is still pending behind the hold"
    );
    assert!(
        events.iter().all(|e| !e.url.ends_with("/@x/video/1")),
        "no event for the challenged page yet: {:?}",
        kinds(&events)
    );
    let hold = store
        .active_holds()
        .unwrap()
        .into_iter()
        .find(|(h, _)| h == "127.0.0.1")
        .map(|(_, until)| until - rayap_core::store::unix_now_ms())
        .expect("host is held");
    assert!(
        hold > 4 * 60 * 1000,
        "held for minutes, not the per-host delay: {hold} ms"
    );
    let pending = store.pending(stats.run).unwrap();
    let challenged = pending
        .iter()
        .find(|p| p.url.ends_with("/@x/video/1"))
        .expect("the challenged page is kept for the next run");
    assert_eq!(challenged.attempts, 1, "one attempt recorded");
    assert!(
        pending
            .iter()
            .all(|p| p.url.ends_with("/@x/video/1") || p.url.contains("/api/comment/list/")),
        "only the challenged page and the comment page behind the same hold wait: {pending:?}"
    );
    // The other post was handled normally before the hold.
    assert_eq!(store.records_after(0, Some("post"), 10).unwrap().len(), 1);
}

#[tokio::test]
async fn a_database_from_before_fingerprint_versions_rebases_once() {
    let site = Site::default();
    site.set(
        "/",
        Reply::Html("<title>Calls</title><p>Deadline 3 Jan</p>".into()),
    );
    let addr = start(site.clone()).await;
    let cfg = config(addr, 10, 0);
    let mut store = Store::open_in_memory().unwrap();
    let (_, events) = run(&cfg, &mut store, Options::default()).await;
    assert_eq!(events[0].kind, EventKind::New);
    let expected = format!("v{}:", rayap_core::html::FINGERPRINT_VERSION);
    assert!(
        events[0].hash.as_deref().unwrap().starts_with(&expected),
        "{:?}",
        events[0].hash
    );

    // Pretend the first fetch was made by a rayap that stored bare hashes (version 1).
    rayap_core::store::test_support::strip_fingerprint_versions(&mut store);

    let (stats, events) = run(&cfg, &mut store, Options::default()).await;
    assert_eq!(events[0].kind, EventKind::Rebaseline, "{:?}", events);
    assert_eq!(stats.changed, 0, "a rebaseline is not a change");
    let (_, events) = run(&cfg, &mut store, Options::default()).await;
    assert_eq!(events[0].kind, EventKind::Unchanged);
}

#[tokio::test]
async fn shards_hand_cross_host_pages_over_and_agree_on_the_end() {
    // Many hosts (127.0.0.1 and localhost resolve to the same server, plus a redirect chain
    // across them) so that with several shards pages must cross shard boundaries. Every
    // outcome must be recorded exactly once and the run must finish.
    let site = Site::default();
    site.set(
        "/",
        Reply::Html("<a href='/a'>a</a><a href='/b'>b</a>".into()),
    );
    site.set("/a", Reply::Html("a".into()));
    site.set("/b", Reply::Redirect("/c".into()));
    site.set("/c", Reply::Html("c".into()));
    let addr = start(site.clone()).await;
    // Two sources on two host names for the same server: they land on different shards
    // (or the same; either way the result must be identical).
    let cfg = Config::from_toml(&format!(
        r#"
        [crawler]
        contact = "mailto:test@example.org"
        per_host_delay_ms = 10
        request_timeout_ms = 5000
        max_attempts = 3
        retry_backoff_ms = 50
        [[source]]
        name = "ip"
        url = "http://{addr}/"
        max_depth = 1
        [[source]]
        name = "name"
        url = "http://localhost:{}/"
        max_depth = 1
        "#,
        addr.port()
    ))
    .unwrap();
    for shards in [1, 2, 3, 8] {
        let mut store = Store::open_in_memory().unwrap();
        let (stats, events) = run(
            &cfg,
            &mut store,
            Options {
                shards,
                ..Options::default()
            },
        )
        .await;
        assert!(stats.finished, "shards={shards}");
        let mut got: Vec<(String, String)> = events
            .iter()
            .map(|e| (e.url.clone(), e.kind.as_str().to_string()))
            .collect();
        got.sort();
        // Each host: /, /a, /b (redirect), /c  => 4 events, twice.
        assert_eq!(got.len(), 8, "shards={shards}: {got:?}");
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == EventKind::Redirect)
                .count(),
            2,
            "shards={shards}"
        );
        assert_eq!(
            stats.requests, 8,
            "shards={shards}: every page fetched exactly once"
        );
        let urls: std::collections::HashSet<&str> = events.iter().map(|e| e.url.as_str()).collect();
        assert_eq!(urls.len(), 8, "shards={shards}: no page recorded twice");
        assert!(
            store.pending(stats.run).unwrap().is_empty(),
            "shards={shards}"
        );
    }
}

#[tokio::test]
async fn a_javascript_shell_is_an_error_not_content() {
    let site = Site::default();
    site.set(
        "/",
        Reply::Html(
            "<!doctype html><html><head><title>Opportunities</title><script src=\"/app.js\"></script></head><body><div id=\"root\"></div></body></html>".into(),
        ),
    );
    site.set(
        "/real",
        Reply::Html("<main><h1>Call</h1><p>Deadline 1 Dec</p></main>".into()),
    );
    let addr = start(site.clone()).await;
    let cfg = Config::from_toml(&format!(
        r#"
        [crawler]
        contact = "mailto:test@example.org"
        per_host_delay_ms = 10
        request_timeout_ms = 5000
        [[source]]
        name = "shell"
        url = "http://{addr}/"
        [[source]]
        name = "real"
        url = "http://{addr}/real"
        "#
    ))
    .unwrap();
    let mut store = Store::open_in_memory().unwrap();
    let (_, events) = run(&cfg, &mut store, Options::default()).await;
    let shell = events.iter().find(|e| e.source == "shell").unwrap();
    assert_eq!(shell.kind, EventKind::Error, "{shell:?}");
    assert!(
        shell
            .detail
            .as_deref()
            .unwrap()
            .contains("application shell")
    );
    assert!(shell.hash.is_none(), "a shell is never fingerprinted");
    let real = events.iter().find(|e| e.source == "real").unwrap();
    assert_eq!(real.kind, EventKind::New);
    assert_eq!(
        store.records_after(0, Some("page"), 10).unwrap().len(),
        1,
        "only the real page has a record"
    );
}

#[tokio::test]
async fn company_source_records_the_organisation_and_its_well_known_pages() {
    let site = Site::default();
    site.set(
        "/",
        Reply::Html(
            r#"<html><head><title>Acme</title><meta name="description" content="We make anvils"></head>
            <body><nav><a href="/about">About</a><a href="/careers">Careers</a><a href="/blog">Blog</a></nav>
            <main><p>Anvils since 1920.</p></main>
            <footer><a href="https://www.linkedin.com/company/acme">LinkedIn</a></footer></body></html>"#
                .into(),
        ),
    );
    site.set(
        "/about",
        Reply::Html("<main><p>Founded in Jakarta.</p></main>".into()),
    );
    site.set(
        "/careers",
        Reply::Html("<main><p>Open role: Rust engineer.</p></main>".into()),
    );
    site.set("/blog", Reply::Html("<main><p>Post one.</p></main>".into()));
    let addr = start(site.clone()).await;
    let cfg = Config::from_toml(&format!(
        r#"
        [crawler]
        contact = "mailto:test@example.org"
        per_host_delay_ms = 10
        request_timeout_ms = 5000
        [[source]]
        name = "acme"
        url = "http://{addr}/"
        kind = "company"
        "#
    ))
    .unwrap();
    let mut store = Store::open_in_memory().unwrap();
    let (stats, events) = run(&cfg, &mut store, Options::default()).await;
    assert!(stats.finished);
    assert_eq!(
        stats.requests,
        4,
        "home + 3 well-known pages: {:?}",
        kinds(&events)
    );
    let company = store.records_after(0, Some("company"), 10).unwrap();
    assert_eq!(company.len(), 1);
    let c = &company[0].data;
    assert_eq!(c["name"], "Acme");
    assert_eq!(c["description"], "We make anvils");
    assert_eq!(c["text"], "Anvils since 1920.");
    assert_eq!(
        c["social"]["linkedin"],
        "https://www.linkedin.com/company/acme"
    );
    assert_eq!(c["pages"].as_object().unwrap().len(), 3);
    let pages = store.records_after(0, Some("company_page"), 10).unwrap();
    let mut roles: Vec<&str> = pages
        .iter()
        .map(|p| p.data["role"].as_str().unwrap())
        .collect();
    roles.sort();
    assert_eq!(roles, ["about", "blog", "careers"]);
    let careers = pages.iter().find(|p| p.data["role"] == "careers").unwrap();
    assert_eq!(careers.data["text"], "Open role: Rust engineer.");

    // Second visit: the careers page gains a role, everything else is unchanged; only the
    // careers record is rewritten.
    site.set(
        "/careers",
        Reply::Html("<main><p>Open role: Rust engineer. Open role: Go engineer.</p></main>".into()),
    );
    let (_, events) = run(&cfg, &mut store, Options::default()).await;
    let changed: Vec<&str> = events
        .iter()
        .filter(|e| e.kind == EventKind::Changed)
        .map(|e| e.url.as_str())
        .collect();
    assert_eq!(changed.len(), 1);
    assert!(changed[0].ends_with("/careers"));
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind == EventKind::Unchanged)
            .count(),
        3
    );
}

#[tokio::test]
async fn a_rendered_source_runs_the_pages_javascript_first() {
    // Skips itself when no browser is installed; rendering is optional.
    if rayap_core::render::Browser::find("rayap/test", std::time::Duration::from_secs(30)).is_none()
    {
        eprintln!("no Chromium/Chrome on PATH; skipping render test");
        return;
    }
    let site = Site::default();
    // Over HTTP this is an empty shell; the script fills it in.
    site.set(
        "/",
        Reply::Html(
            r#"<!doctype html><html><head><title>Opps</title></head><body><main id="root"></main>
            <script>document.getElementById('root').innerHTML = '<h1>Grant</h1><p>Deadline 1 Dec 2026</p>';</script>
            </body></html>"#
                .into(),
        ),
    );
    let addr = start(site.clone()).await;
    let cfg = |render: bool| {
        Config::from_toml(&format!(
            r#"
            [crawler]
            contact = "mailto:test@example.org"
            per_host_delay_ms = 10
            request_timeout_ms = 30000
            [[source]]
            name = "opps"
            url = "http://{addr}/"
            render = {render}
            "#
        ))
        .unwrap()
    };
    let mut plain = Store::open_in_memory().unwrap();
    let (_, events) = run(&cfg(false), &mut plain, Options::default()).await;
    assert_eq!(
        events[0].kind,
        EventKind::Error,
        "plain HTTP sees a shell: {:?}",
        events[0]
    );

    let mut rendered = Store::open_in_memory().unwrap();
    let (_, events) = run(&cfg(true), &mut rendered, Options::default()).await;
    assert_eq!(events[0].kind, EventKind::New, "{:?}", events[0]);
    let page = &rendered.records_after(0, Some("page"), 10).unwrap()[0];
    assert_eq!(page.data["text"], "Grant Deadline 1 Dec 2026");
    // The browser identified itself as rayap and respected the host queue (robots first).
    let agents = site.user_agents();
    assert!(agents.iter().all(|ua| ua.contains("rayap/")), "{agents:?}");
    assert_eq!(site.requests()[0], "/robots.txt");
}

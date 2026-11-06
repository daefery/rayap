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

async fn run(
    config: &Config,
    store: &mut Store,
    options: Options,
) -> (rayap_core::Stats, Vec<Event>) {
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

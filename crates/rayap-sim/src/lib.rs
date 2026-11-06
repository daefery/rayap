//! A simulated web for benchmarking crawlers on one machine.
//!
//! One HTTP server answers for any number of virtual hosts (`h00000.sim`, `h00001.sim`, ...),
//! told apart by the `Host` header. Each host has a robots.txt, a root page and `pages` more
//! pages linked to each other, plus deterministic trouble: slow hosts, latency drawn from a
//! log-normal distribution, transient 500s, 429s with Retry-After, 404s, redirects, robots.txt
//! rules, Crawl-delay and robots.txt files that fail once.
//!
//! The server is also the referee. It watches every request and counts politeness
//! violations independently of the crawler: two requests in flight to one host, a request
//! sooner than the host's delay after the previous response ended, a request before a
//! Retry-After expired, a page requested before robots.txt, and a disallowed path requested.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde::Serialize;

/// The shape of the simulated web. All percentages are 0-100.
#[derive(Debug, Clone, Serialize, clap::Args)]
pub struct WebSpec {
    #[arg(long, default_value_t = 10_000)]
    pub hosts: u32,
    /// Pages per host besides the root page.
    #[arg(long, default_value_t = 20)]
    pub pages: u32,
    /// Links from each page to other pages on the same host.
    #[arg(long, default_value_t = 5)]
    pub links: u32,
    /// Size of each HTML page in KiB.
    #[arg(long, default_value_t = 30)]
    pub body_kib: u32,
    /// Median response latency (log-normal).
    #[arg(long, default_value_t = 80)]
    pub latency_median_ms: u64,
    /// Log-normal sigma; 1.0 puts p99 near 10x the median.
    #[arg(long, default_value_t = 1.0)]
    pub latency_sigma: f64,
    /// Hosts whose every response is 10x slower.
    #[arg(long, default_value_t = 2.0)]
    pub slow_hosts_pct: f64,
    /// Pages whose first request gets a 500.
    #[arg(long, default_value_t = 1.0)]
    pub error_pct: f64,
    /// Pages whose first request gets a 429 with Retry-After.
    #[arg(long, default_value_t = 0.5)]
    pub throttle_pct: f64,
    #[arg(long, default_value_t = 5)]
    pub retry_after_s: u64,
    /// Pages that are 404.
    #[arg(long, default_value_t = 1.0)]
    pub gone_pct: f64,
    /// Pages that redirect (301) to another URL on the same host.
    #[arg(long, default_value_t = 1.0)]
    pub redirect_pct: f64,
    /// Hosts whose robots.txt disallows /private (every page links there).
    #[arg(long, default_value_t = 10.0)]
    pub disallow_hosts_pct: f64,
    /// Hosts whose robots.txt sets a Crawl-delay.
    #[arg(long, default_value_t = 2.0)]
    pub crawl_delay_hosts_pct: f64,
    #[arg(long, default_value_t = 4)]
    pub crawl_delay_s: u64,
    /// Hosts with no robots.txt (404).
    #[arg(long, default_value_t = 20.0)]
    pub robots_missing_pct: f64,
    /// Hosts whose robots.txt answers 503 the first time.
    #[arg(long, default_value_t = 1.0)]
    pub robots_flaky_pct: f64,
    /// The per-host delay the crawler promises, used to judge politeness.
    #[arg(long, default_value_t = 2000)]
    pub delay_ms: u64,
    #[arg(long, default_value_t = 1)]
    pub seed: u64,
    /// Comment lines appended to every robots.txt, in KiB (tests use it to make robots.txt
    /// too large to deliver while the client is not reading).
    #[arg(long, default_value_t = 0)]
    pub robots_padding_kib: u32,
}

impl WebSpec {
    /// Every URL a polite crawler fetches successfully, given the spec (approximate: some
    /// pages are unreachable because they are only linked from pages that 404 or redirect).
    pub fn root_urls(&self) -> impl Iterator<Item = String> + '_ {
        (0..self.hosts).map(|h| format!("http://{}/", host_name(h)))
    }
}

pub fn host_name(h: u32) -> String {
    format!("h{h:05}.sim")
}

/// Loopback address for host `h`, so each simulated host has its own IP (127.1.0.1 ...).
pub fn host_ip(h: u32) -> std::net::Ipv4Addr {
    let n = h + 1;
    std::net::Ipv4Addr::new(127, 1 + (n >> 16) as u8, (n >> 8) as u8, n as u8)
}

pub fn parse_host(name: &str) -> Option<u32> {
    let name = name.split(':').next()?;
    name.strip_prefix('h')?.strip_suffix(".sim")?.parse().ok()
}

/// SplitMix64: a fast, well-mixed hash used for every deterministic choice.
pub fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

fn hash(parts: &[u64]) -> u64 {
    parts.iter().fold(0x5EED, |acc, p| mix(acc ^ mix(*p)))
}

/// Uniform in [0, 1).
fn unit(h: u64) -> f64 {
    (h >> 11) as f64 / (1u64 << 53) as f64
}

fn chance(pct: f64, h: u64) -> bool {
    unit(h) * 100.0 < pct
}

fn path_key(path: &str) -> u64 {
    path.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3)
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    Html(String),
    Text(String),
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: Body,
    pub latency: Duration,
}

/// Per-host facts derived from the spec.
struct HostFacts {
    slow: bool,
    disallow_private: bool,
    crawl_delay: Option<u64>,
    robots_missing: bool,
    robots_flaky: bool,
}

impl WebSpec {
    fn facts(&self, h: u32) -> HostFacts {
        let k = |salt: u64| hash(&[self.seed, u64::from(h), salt]);
        let robots_missing = chance(self.robots_missing_pct, k(4));
        // A host without robots.txt has no rules: nothing is disallowed and no Crawl-delay.
        HostFacts {
            slow: chance(self.slow_hosts_pct, k(1)),
            disallow_private: !robots_missing && chance(self.disallow_hosts_pct, k(2)),
            crawl_delay: (!robots_missing && chance(self.crawl_delay_hosts_pct, k(3)))
                .then_some(self.crawl_delay_s),
            robots_missing,
            robots_flaky: chance(self.robots_flaky_pct, k(5)),
        }
    }

    /// The delay a polite crawler must leave between requests to host `h`.
    pub fn required_gap(&self, h: u32) -> Duration {
        let crawl_delay = self.facts(h).crawl_delay.map_or(0, |s| s * 1000);
        Duration::from_millis(self.delay_ms.max(crawl_delay))
    }

    pub fn disallows(&self, h: u32, path: &str) -> bool {
        path.starts_with("/private") && self.facts(h).disallow_private
    }

    fn latency(&self, h: u32, path: &str, attempt: u32) -> Duration {
        let r = hash(&[
            self.seed,
            u64::from(h),
            path_key(path),
            u64::from(attempt),
            7,
        ]);
        // Box-Muller from two uniforms.
        let (u1, u2) = (unit(r).max(1e-12), unit(mix(r)));
        let z = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos();
        let mut ms = self.latency_median_ms as f64 * (self.latency_sigma * z).exp();
        if self.facts(h).slow {
            ms *= 10.0;
        }
        Duration::from_micros((ms.min(10_000.0) * 1000.0) as u64)
    }

    /// The response for `path` on host `h`, on the `attempt`-th request for that path (0-based).
    pub fn respond(&self, h: u32, path: &str, attempt: u32, filler: &str) -> Response {
        let facts = self.facts(h);
        let latency = self.latency(h, path, attempt);
        let k = |salt: u64| hash(&[self.seed, u64::from(h), path_key(path), salt]);
        let plain = |status: u16| Response {
            status,
            headers: vec![],
            body: Body::Empty,
            latency,
        };

        if path == "/robots.txt" {
            if facts.robots_flaky && attempt == 0 {
                return plain(503);
            }
            if facts.robots_missing {
                return plain(404);
            }
            let mut txt = String::from("User-agent: *\n");
            if facts.disallow_private {
                txt.push_str("Disallow: /private\n");
            } else {
                txt.push_str("Disallow: /cgi-bin/\n");
            }
            if let Some(d) = facts.crawl_delay {
                txt.push_str(&format!("Crawl-delay: {d}\n"));
            }
            while txt.len() < self.robots_padding_kib as usize * 1024 {
                txt.push_str("# padding padding padding padding padding padding padding\n");
            }
            return Response {
                status: 200,
                headers: vec![],
                body: Body::Text(txt),
                latency,
            };
        }

        let page: Option<u32> = if path == "/" {
            None
        } else if let Some(rest) = path.strip_prefix("/p/") {
            match rest.split('?').next().and_then(|n| n.parse().ok()) {
                Some(i) if i < self.pages => Some(i),
                _ => return plain(404),
            }
        } else if path.starts_with("/private") {
            Some(u32::MAX)
        } else {
            return plain(404);
        };

        if page.is_some() && !path.contains('?') {
            if chance(self.gone_pct, k(10)) {
                return plain(404);
            }
            if chance(self.redirect_pct, k(11)) {
                let to = format!("{path}?moved=1");
                return Response {
                    status: 301,
                    headers: vec![("location", to)],
                    body: Body::Empty,
                    latency,
                };
            }
        }
        if attempt == 0 && chance(self.error_pct, k(12)) {
            return plain(500);
        }
        if attempt == 0 && chance(self.throttle_pct, k(13)) {
            return Response {
                status: 429,
                headers: vec![("retry-after", self.retry_after_s.to_string())],
                body: Body::Empty,
                latency,
            };
        }

        let index = page.unwrap_or(u32::MAX);
        let mut html = String::with_capacity(self.body_kib as usize * 1024 + 2048);
        html.push_str(&format!(
            "<!doctype html><html><head><title>{} page {}</title><style>p{{margin:0}}</style></head><body><h1>Page {}</h1><ul>",
            host_name(h),
            path,
            path
        ));
        let mut link =
            |href: String| html.push_str(&format!("<li><a href=\"{href}\">{href}</a></li>"));
        if page.is_none() {
            for i in 0..self.pages {
                link(format!("/p/{i}"));
            }
        } else if self.pages > 0 {
            for j in 0..self.links {
                let target = mix(k(20 + u64::from(j))) % u64::from(self.pages);
                link(format!("/p/{target}"));
            }
        }
        link(format!("/private/{}", index));
        let other = (mix(k(30)) % u64::from(self.hosts.max(1))) as u32;
        link(format!("http://{}/", host_name(other)));
        html.push_str("</ul><p>");
        let want = (self.body_kib as usize * 1024).saturating_sub(html.len() + 64);
        html.push_str(&filler[..want.min(filler.len())]);
        html.push_str("</p><script>var v=1;</script></body></html>");
        Response {
            status: 200,
            headers: vec![],
            body: Body::Html(html),
            latency,
        }
    }
}

/// Filler text for page bodies.
pub fn filler(len: usize) -> String {
    const WORDS: &[&str] = &[
        "grant",
        "deadline",
        "proposal",
        "funding",
        "research",
        "community",
        "eligibility",
        "budget",
        "review",
        "application",
        "program",
        "culture",
        "story",
        "history",
        "award",
        "panel",
    ];
    let mut out = String::with_capacity(len + 16);
    let mut i = 0u64;
    while out.len() < len {
        out.push_str(WORDS[(mix(i) % WORDS.len() as u64) as usize]);
        out.push(' ');
        i += 1;
    }
    out.truncate(len);
    out
}

#[derive(Default)]
struct HostWatch {
    in_flight: u32,
    /// The previous response ended somewhere in [earliest, latest]: after its last byte was
    /// written and the last time unacknowledged bytes were seen, and no later than the first
    /// time the send queue was seen empty.
    last_end: Option<(Instant, Instant)>,
    last_path: String,
    last_status: u16,
    /// robots.txt has been served with 200, so the crawler knows its Crawl-delay.
    rules_served: bool,
    /// Retry-After expiry, as a window matching the end window of the 429 response.
    not_before: Option<(Instant, Instant)>,
    /// What the last completed robots.txt response permits.
    robots: RobotsSeen,
    attempts: HashMap<String, u32>,
    requests: u64,
    min_gap_slack_ms: Option<i64>,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum RobotsSeen {
    /// No robots.txt response has completed yet: no page may be requested.
    #[default]
    None,
    /// The last robots.txt response was 5xx/429/network: no page may be requested (RFC 9309).
    Unreachable,
    /// 2xx or 4xx: pages may be requested, subject to the rules.
    Usable,
}

#[derive(Debug, Default, Serialize)]
pub struct Violations {
    /// A request arrived while another request to the same host was in flight.
    pub concurrent: u64,
    /// A request arrived sooner than the required delay after the previous response ended.
    pub too_soon: u64,
    /// A request arrived before a Retry-After the host sent had expired.
    pub retry_after: u64,
    /// A page was requested before a usable robots.txt response had been fully sent
    /// (none yet, or the last one was a server error).
    pub before_robots: u64,
    /// A path disallowed by robots.txt was requested.
    pub disallowed: u64,
}

/// Requests the referee could neither prove polite nor prove impolite, because the previous
/// response's true end lies inside its observation window. Not counted as violations.
#[derive(Debug, Default, Serialize)]
pub struct Uncertain {
    pub too_soon: u64,
    pub retry_after: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub requests: u64,
    pub robots_requests: u64,
    pub status_200: u64,
    pub bytes_sent: u64,
    pub unknown_host: u64,
    /// Responses the client did not fully receive (disconnect or timeout).
    pub aborted: u64,
    pub hosts_contacted: u64,
    /// Proven violations: impolite even if each previous response ended as early as possible.
    pub violations: Violations,
    pub uncertain: Uncertain,
    /// Smallest (gap - required delay) seen on any host, in ms, taking the latest possible end
    /// of the previous response. Negative only means "not proven polite"; see `uncertain`.
    pub min_gap_slack_ms: Option<i64>,
    /// Width of the window in which each response's end was located (polling resolution).
    pub end_window_ms_max: f64,
    pub end_window_ms_mean: f64,
    /// The first violations, with enough context to reproduce them.
    pub details: Vec<String>,
}

/// Tracks every request per host and counts politeness violations.
pub struct Referee {
    hosts: Vec<Mutex<HostWatch>>,
    details: Mutex<Vec<String>>,
    requests: AtomicU64,
    robots_requests: AtomicU64,
    ok: AtomicU64,
    bytes: AtomicU64,
    unknown: AtomicU64,
    aborted: AtomicU64,
    concurrent: AtomicU64,
    too_soon: AtomicU64,
    retry_after: AtomicU64,
    before_robots: AtomicU64,
    disallowed: AtomicU64,
    uncertain_too_soon: AtomicU64,
    uncertain_retry_after: AtomicU64,
    end_window_us_max: AtomicU64,
    end_window_us_sum: AtomicU64,
    ends: AtomicU64,
}

/// Slack for clock granularity when judging gaps.
const TOLERANCE: Duration = Duration::from_millis(1);

impl Referee {
    pub fn new(hosts: u32) -> Self {
        Self {
            hosts: (0..hosts)
                .map(|_| Mutex::new(HostWatch::default()))
                .collect(),
            details: Mutex::new(Vec::new()),
            requests: AtomicU64::new(0),
            robots_requests: AtomicU64::new(0),
            ok: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            unknown: AtomicU64::new(0),
            aborted: AtomicU64::new(0),
            concurrent: AtomicU64::new(0),
            too_soon: AtomicU64::new(0),
            retry_after: AtomicU64::new(0),
            before_robots: AtomicU64::new(0),
            disallowed: AtomicU64::new(0),
            uncertain_too_soon: AtomicU64::new(0),
            uncertain_retry_after: AtomicU64::new(0),
            end_window_us_max: AtomicU64::new(0),
            end_window_us_sum: AtomicU64::new(0),
            ends: AtomicU64::new(0),
        }
    }

    pub fn unknown_host(&self) {
        self.unknown.fetch_add(1, Ordering::Relaxed);
    }

    /// Records the start of a request and returns how many times this path was requested before.
    pub fn start(&self, spec: &WebSpec, h: u32, path: &str, now: Instant) -> u32 {
        self.requests.fetch_add(1, Ordering::Relaxed);
        let mut w = self.hosts[h as usize].lock().unwrap();
        w.requests += 1;
        if w.in_flight > 0 {
            self.concurrent.fetch_add(1, Ordering::Relaxed);
            self.note(format!(
                "concurrent host={h} path={path} other={}",
                w.last_path
            ));
        }
        if let Some((earliest, latest)) = w.last_end {
            // Longest and shortest possible gap since the previous response ended.
            let gap_max = now.saturating_duration_since(earliest);
            let gap_min = now.saturating_duration_since(latest);
            // Crawl-delay only binds once the crawler could have read it.
            let required = if w.rules_served {
                spec.required_gap(h)
            } else {
                Duration::from_millis(spec.delay_ms)
            };
            let slack = gap_min.as_millis() as i64 - required.as_millis() as i64;
            w.min_gap_slack_ms = Some(w.min_gap_slack_ms.map_or(slack, |m| m.min(slack)));
            if gap_max + TOLERANCE < required {
                self.too_soon.fetch_add(1, Ordering::Relaxed);
                self.note(format!(
                    "too_soon host={h} path={path} gap_ms={:.1}..{:.1} required_ms={} previous={} status={}",
                    gap_min.as_secs_f64() * 1e3,
                    gap_max.as_secs_f64() * 1e3,
                    required.as_millis(),
                    w.last_path,
                    w.last_status
                ));
            } else if gap_min < required {
                self.uncertain_too_soon.fetch_add(1, Ordering::Relaxed);
            }
        }
        if let Some((nb_earliest, nb_latest)) = w.not_before {
            if now + TOLERANCE < nb_earliest {
                self.retry_after.fetch_add(1, Ordering::Relaxed);
                self.note(format!("retry_after host={h} path={path}"));
            } else if now < nb_latest {
                self.uncertain_retry_after.fetch_add(1, Ordering::Relaxed);
            }
        }
        if path == "/robots.txt" {
            self.robots_requests.fetch_add(1, Ordering::Relaxed);
        } else {
            if w.robots != RobotsSeen::Usable {
                self.before_robots.fetch_add(1, Ordering::Relaxed);
                self.note(format!("before_robots host={h} path={path}"));
            }
            if spec.disallows(h, path) {
                self.disallowed.fetch_add(1, Ordering::Relaxed);
                self.note(format!("disallowed host={h} path={path}"));
            }
        }
        w.in_flight += 1;
        w.last_path = path.to_string();
        let attempt = w.attempts.entry(path.to_string()).or_insert(0);
        let this = *attempt;
        *attempt += 1;
        this
    }

    /// Records the end of a response for `path`. Its true end (the client acknowledging the
    /// last byte, or delivery failing) lies in [`earliest`, `latest`]. The gap and
    /// Retry-After clocks start there. A robots.txt that was not delivered leaves the host
    /// without usable rules.
    #[allow(clippy::too_many_arguments)]
    pub fn end(
        &self,
        h: u32,
        path: &str,
        response: &Response,
        bytes: usize,
        earliest: Instant,
        latest: Instant,
        delivered: bool,
    ) {
        let latest = latest.max(earliest);
        let window_us = (latest - earliest).as_micros() as u64;
        self.end_window_us_max
            .fetch_max(window_us, Ordering::Relaxed);
        self.end_window_us_sum
            .fetch_add(window_us, Ordering::Relaxed);
        self.ends.fetch_add(1, Ordering::Relaxed);
        let mut w = self.hosts[h as usize].lock().unwrap();
        w.in_flight -= 1;
        w.last_end = Some((earliest, latest));
        w.last_status = response.status;
        if !delivered {
            self.aborted.fetch_add(1, Ordering::Relaxed);
        }
        if path == "/robots.txt" {
            w.robots = match response.status {
                _ if !delivered => RobotsSeen::Unreachable,
                200..=299 | 400..=428 | 430..=499 => RobotsSeen::Usable,
                _ => RobotsSeen::Unreachable,
            };
            if response.status == 200 && delivered {
                w.rules_served = true;
            }
        }
        if response.status == 429 {
            let secs = response
                .headers
                .iter()
                .find(|(k, _)| *k == "retry-after")
                .and_then(|(_, v)| v.parse().ok());
            if let Some(secs) = secs {
                let wait = Duration::from_secs(secs);
                w.not_before = Some((earliest + wait, latest + wait));
            }
        }
        if response.status == 200 {
            self.ok.fetch_add(1, Ordering::Relaxed);
        }
        self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    fn note(&self, line: String) {
        let mut d = self.details.lock().unwrap();
        if d.len() < 50 {
            d.push(line);
        }
    }

    pub fn report(&self) -> Report {
        let mut contacted = 0;
        let mut min_slack: Option<i64> = None;
        for host in &self.hosts {
            let w = host.lock().unwrap();
            if w.requests > 0 {
                contacted += 1;
            }
            if let Some(s) = w.min_gap_slack_ms {
                min_slack = Some(min_slack.map_or(s, |m| m.min(s)));
            }
        }
        let get = |a: &AtomicU64| a.load(Ordering::Relaxed);
        Report {
            requests: get(&self.requests),
            robots_requests: get(&self.robots_requests),
            status_200: get(&self.ok),
            bytes_sent: get(&self.bytes),
            unknown_host: get(&self.unknown),
            aborted: get(&self.aborted),
            uncertain: Uncertain {
                too_soon: get(&self.uncertain_too_soon),
                retry_after: get(&self.uncertain_retry_after),
            },
            end_window_ms_max: get(&self.end_window_us_max) as f64 / 1e3,
            end_window_ms_mean: get(&self.end_window_us_sum) as f64
                / 1e3
                / get(&self.ends).max(1) as f64,
            hosts_contacted: contacted,
            violations: Violations {
                concurrent: get(&self.concurrent),
                too_soon: get(&self.too_soon),
                retry_after: get(&self.retry_after),
                before_robots: get(&self.before_robots),
                disallowed: get(&self.disallowed),
            },
            min_gap_slack_ms: min_slack,
            details: self.details.lock().unwrap().clone(),
        }
    }
}

/// The simulated web over plain HTTP/1.1 on TCP, written directly on sockets so the referee
/// sees transport events: a response ends when the client's TCP stack has acknowledged its
/// last byte (the kernel send queue is empty), or when delivery fails.
pub mod server {
    use std::os::fd::{AsRawFd, RawFd};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};

    use super::{Body, Referee, WebSpec, filler, parse_host};

    /// Immediate re-checks of the send queue before polling on a timer.
    const SPIN_CHECKS: u32 = 64;
    /// How long a response may take to be acknowledged before it counts as failed.
    const DELIVERY_TIMEOUT: Duration = Duration::from_secs(30);
    const MAX_HEAD_BYTES: usize = 16 * 1024;

    pub struct Sim {
        pub spec: WebSpec,
        pub referee: Referee,
        filler: String,
        /// SO_SNDBUF for accepted sockets (tests use a small one to force backpressure).
        send_buffer: Option<usize>,
    }

    impl Sim {
        pub fn new(spec: WebSpec) -> Self {
            Self::with_send_buffer(spec, None)
        }

        pub fn with_send_buffer(spec: WebSpec, send_buffer: Option<usize>) -> Self {
            Self {
                referee: Referee::new(spec.hosts),
                filler: filler(spec.body_kib as usize * 1024 + 1024),
                spec,
                send_buffer,
            }
        }
    }

    pub async fn serve(listener: TcpListener, sim: Arc<Sim>) {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                tokio::time::sleep(Duration::from_millis(10)).await;
                continue;
            };
            let _ = stream.set_nodelay(true);
            if let Some(size) = sim.send_buffer {
                set_send_buffer(stream.as_raw_fd(), size);
            }
            tokio::spawn(connection(stream, Arc::clone(&sim)));
        }
    }

    fn set_send_buffer(fd: RawFd, size: usize) {
        let size = size as libc::c_int;
        // SAFETY: fd is an open socket owned by the caller; the option value is a c_int.
        unsafe {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                (&size as *const libc::c_int).cast(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }

    /// Bytes written but not yet acknowledged by the peer (Linux SIOCOUTQ / TIOCOUTQ).
    fn unacked_bytes(fd: RawFd) -> Option<i32> {
        let mut n: libc::c_int = 0;
        // SAFETY: fd is an open socket; TIOCOUTQ writes one c_int.
        let rc = unsafe { libc::ioctl(fd, libc::TIOCOUTQ, &mut n) };
        (rc == 0).then_some(n)
    }

    /// Waits until every byte written to `fd` has been acknowledged, starting at `written`
    /// (when the last byte was handed to the kernel). Returns whether delivery succeeded and
    /// the window [earliest, latest] that contains the moment it happened (or failed): after
    /// the last observation of unacknowledged bytes, no later than the first empty one.
    async fn delivered(fd: RawFd, written: Instant) -> (bool, Instant, Instant) {
        let deadline = written + DELIVERY_TIMEOUT;
        let mut earliest = written;
        let mut spins = 0;
        loop {
            let observed = Instant::now();
            match unacked_bytes(fd) {
                Some(0) => return (true, earliest, observed),
                Some(_) if observed < deadline => {
                    earliest = observed;
                    // Most responses are acknowledged within microseconds on loopback: check
                    // again right away a few times before falling back to a 1 ms timer, so the
                    // window that brackets the true end stays narrow.
                    if spins < SPIN_CHECKS {
                        spins += 1;
                        tokio::task::yield_now().await;
                    } else {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                }
                _ => return (false, earliest, observed),
            }
        }
    }

    struct Head {
        path: String,
        host: String,
        close: bool,
        get: bool,
    }

    /// Reads one request head. None when the connection closes or the request is malformed.
    async fn read_head<R: tokio::io::AsyncBufRead + Unpin>(reader: &mut R) -> Option<Head> {
        let mut line = String::new();
        let mut total = 0;
        let mut head: Option<Head> = None;
        loop {
            line.clear();
            let n = reader.read_line(&mut line).await.ok()?;
            if n == 0 {
                return None;
            }
            total += n;
            if total > MAX_HEAD_BYTES {
                return None;
            }
            let l = line.trim_end_matches(['\r', '\n']);
            match head.as_mut() {
                None => {
                    let mut parts = l.split(' ');
                    let method = parts.next()?;
                    let path = parts.next()?.to_string();
                    let version = parts.next().unwrap_or("HTTP/1.1");
                    head = Some(Head {
                        path,
                        host: String::new(),
                        close: version == "HTTP/1.0",
                        get: method == "GET",
                    });
                }
                Some(h) => {
                    if l.is_empty() {
                        return head;
                    }
                    if let Some((k, v)) = l.split_once(':') {
                        let v = v.trim();
                        if k.eq_ignore_ascii_case("host") {
                            h.host = v.to_string();
                        } else if k.eq_ignore_ascii_case("connection") {
                            h.close = v.eq_ignore_ascii_case("close");
                        }
                    }
                }
            }
        }
    }

    fn reason(status: u16) -> &'static str {
        match status {
            200 => "OK",
            301 => "Moved Permanently",
            404 => "Not Found",
            405 => "Method Not Allowed",
            421 => "Misdirected Request",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            503 => "Service Unavailable",
            _ => "Status",
        }
    }

    fn encode(
        status: u16,
        content_type: &str,
        extra: &[(&str, String)],
        body: &[u8],
        close: bool,
    ) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.1 {status} {}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n",
            reason(status),
            body.len()
        );
        for (k, v) in extra {
            out.push_str(&format!("{k}: {v}\r\n"));
        }
        if close {
            out.push_str("connection: close\r\n");
        }
        out.push_str("\r\n");
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(body);
        bytes
    }

    async fn connection(stream: TcpStream, sim: Arc<Sim>) {
        let fd = stream.as_raw_fd();
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        while let Some(head) = read_head(&mut reader).await {
            let simulated = parse_host(&head.host).filter(|h| *h < sim.spec.hosts);
            let Some(h) = simulated.filter(|_| head.get) else {
                // Control endpoints and stray requests: not refereed.
                let (status, body) = match (head.get, head.path.as_str()) {
                    (true, "/__sim/report") => (
                        200,
                        serde_json::to_vec(&sim.referee.report()).unwrap_or_default(),
                    ),
                    (true, "/__sim/spec") => {
                        (200, serde_json::to_vec(&sim.spec).unwrap_or_default())
                    }
                    (false, _) => (405, Vec::new()),
                    _ => {
                        sim.referee.unknown_host();
                        (421, Vec::new())
                    }
                };
                let bytes = encode(status, "application/json", &[], &body, head.close);
                if write_half.write_all(&bytes).await.is_err() || head.close {
                    return;
                }
                continue;
            };

            let attempt = sim.referee.start(&sim.spec, h, &head.path, Instant::now());
            let response = sim.spec.respond(h, &head.path, attempt, &sim.filler);
            tokio::time::sleep(response.latency).await;
            let (content_type, body): (&str, &[u8]) = match &response.body {
                Body::Html(s) => ("text/html; charset=utf-8", s.as_bytes()),
                Body::Text(s) => ("text/plain", s.as_bytes()),
                Body::Empty => ("text/plain", &[]),
            };
            let bytes = encode(
                response.status,
                content_type,
                &response.headers,
                body,
                head.close,
            );
            let (ok, earliest, latest) = if write_all(&mut write_half, &bytes).await {
                delivered(fd, Instant::now()).await
            } else {
                let now = Instant::now();
                (false, now, now)
            };
            sim.referee
                .end(h, &head.path, &response, body.len(), earliest, latest, ok);
            if !ok || head.close {
                return;
            }
        }
    }

    async fn write_all(w: &mut tokio::net::tcp::OwnedWriteHalf, bytes: &[u8]) -> bool {
        tokio::time::timeout(DELIVERY_TIMEOUT, w.write_all(bytes))
            .await
            .is_ok_and(|r| r.is_ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        spec: WebSpec,
    }

    fn spec() -> WebSpec {
        Cli::parse_from(["sim"]).spec
    }

    #[test]
    fn host_names_round_trip_and_ips_are_distinct() {
        assert_eq!(parse_host(&host_name(42)), Some(42));
        assert_eq!(parse_host("h00042.sim:8080"), Some(42));
        assert_eq!(parse_host("example.org"), None);
        assert_ne!(host_ip(0), host_ip(1));
        assert_eq!(host_ip(65535).octets(), [127, 2, 0, 0]);
    }

    #[test]
    fn responses_are_deterministic_and_sized() {
        let s = spec();
        let f = filler(64 * 1024);
        let a = s.respond(7, "/", 1, &f);
        assert_eq!(a, s.respond(7, "/", 1, &f));
        let Body::Html(html) = &a.body else {
            panic!("{a:?}")
        };
        assert!(
            (29 * 1024..=31 * 1024).contains(&html.len()),
            "{}",
            html.len()
        );
        assert!(html.contains("href=\"/p/19\""));
    }

    #[test]
    fn trouble_rates_are_roughly_as_specified() {
        let s = spec();
        let f = filler(64 * 1024);
        let mut counts: HashMap<u16, u32> = HashMap::new();
        let n = 2000;
        for h in 0..n {
            for i in 0..s.pages {
                *counts
                    .entry(s.respond(h, &format!("/p/{i}"), 0, &f).status)
                    .or_default() += 1;
            }
        }
        let total = (n * s.pages) as f64;
        let pct = |status| f64::from(*counts.get(&status).unwrap_or(&0)) * 100.0 / total;
        assert!((0.7..1.3).contains(&pct(404)), "404 {}", pct(404));
        assert!((0.7..1.3).contains(&pct(301)), "301 {}", pct(301));
        assert!((0.7..1.3).contains(&pct(500)), "500 {}", pct(500));
        assert!((0.3..0.7).contains(&pct(429)), "429 {}", pct(429));
    }

    #[test]
    fn rules_exist_only_where_robots_txt_does() {
        let s = spec();
        let f = filler(16);
        for h in 0..5000 {
            let robots = s.respond(h, "/robots.txt", 1, &f);
            let text = match &robots.body {
                Body::Text(t) => t.clone(),
                _ => String::new(),
            };
            assert_eq!(
                s.disallows(h, "/private/1"),
                text.contains("Disallow: /private"),
                "host {h}"
            );
            let has_delay = s.required_gap(h) > Duration::from_millis(s.delay_ms);
            assert_eq!(has_delay, text.contains("Crawl-delay"), "host {h}");
        }
    }

    #[test]
    fn referee_counts_violations() {
        let s = WebSpec {
            delay_ms: 100,
            ..spec()
        };
        let r = Referee::new(1);
        let ok = Response {
            status: 200,
            headers: vec![],
            body: Body::Empty,
            latency: Duration::ZERO,
        };
        let t0 = Instant::now();
        r.start(&s, 0, "/p/1", t0); // before robots
        r.start(&s, 0, "/robots.txt", t0); // concurrent
        r.end(0, "/p/1", &ok, 0, t0, t0, true);
        r.end(0, "/robots.txt", &ok, 0, t0, t0, true);
        r.start(&s, 0, "/p/2", t0 + Duration::from_millis(50)); // too soon
        r.end(
            0,
            "/p/2",
            &ok,
            0,
            t0 + Duration::from_millis(50),
            t0 + Duration::from_millis(50),
            true,
        );
        r.start(&s, 0, "/p/3", t0 + Duration::from_millis(200)); // fine
        let v = r.report().violations;
        assert_eq!(
            (v.before_robots, v.concurrent, v.too_soon, v.retry_after),
            (1, 1, 1, 0)
        );
    }

    #[test]
    fn crawl_delay_binds_only_after_robots_txt_was_served() {
        let base = spec();
        let h = (0..5000)
            .find(|h| base.required_gap(*h) > Duration::from_millis(base.delay_ms))
            .unwrap();
        let s = WebSpec {
            delay_ms: 100,
            ..base
        };
        let r = Referee::new(h + 1);
        let failed = Response {
            status: 503,
            headers: vec![],
            body: Body::Empty,
            latency: Duration::ZERO,
        };
        let ok = Response {
            status: 200,
            ..failed.clone()
        };
        let t0 = Instant::now();
        r.start(&s, h, "/robots.txt", t0);
        r.end(h, "/robots.txt", &failed, 0, t0, t0, true);
        // Only the base delay applies: the crawler has not seen the Crawl-delay yet.
        r.start(&s, h, "/robots.txt", t0 + Duration::from_millis(150));
        r.end(
            h,
            "/robots.txt",
            &ok,
            0,
            t0 + Duration::from_millis(150),
            t0 + Duration::from_millis(150),
            true,
        );
        assert_eq!(r.report().violations.too_soon, 0);
        // Now the Crawl-delay applies.
        r.start(&s, h, "/", t0 + Duration::from_millis(300));
        assert_eq!(r.report().violations.too_soon, 1);
    }

    fn quiet_spec(delay_ms: u64, body_kib: u32) -> WebSpec {
        WebSpec {
            hosts: 1,
            pages: 1,
            body_kib,
            latency_median_ms: 1,
            latency_sigma: 0.0,
            slow_hosts_pct: 0.0,
            error_pct: 0.0,
            throttle_pct: 0.0,
            gone_pct: 0.0,
            redirect_pct: 0.0,
            robots_missing_pct: 0.0,
            robots_flaky_pct: 0.0,
            disallow_hosts_pct: 0.0,
            crawl_delay_hosts_pct: 0.0,
            delay_ms,
            ..spec()
        }
    }

    async fn start(sim: std::sync::Arc<server::Sim>) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(server::serve(listener, sim));
        addr
    }

    /// Sends a GET and returns the connected socket without reading the response.
    async fn send(
        addr: std::net::SocketAddr,
        path: &str,
        recv_buffer: Option<usize>,
    ) -> tokio::net::TcpStream {
        use tokio::io::AsyncWriteExt;
        let socket = tokio::net::TcpSocket::new_v4().unwrap();
        if let Some(size) = recv_buffer {
            socket.set_recv_buffer_size(size as u32).unwrap();
        }
        let mut stream = socket.connect(addr).await.unwrap();
        let req = format!("GET {path} HTTP/1.1\r\nHost: h00000.sim\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).await.unwrap();
        stream
    }

    async fn read_all(mut stream: tokio::net::TcpStream) -> Vec<u8> {
        use tokio::io::AsyncReadExt;
        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        buf
    }

    /// Negative control for transport backpressure: the client stops reading a large body, so
    /// the server's writes block. A second request on another connection while the first body
    /// is undelivered must count as concurrent, and the gap after it must be measured from the
    /// moment the body is finally acknowledged.
    #[tokio::test]
    async fn referee_waits_for_the_client_to_receive_the_body() {
        use std::sync::Arc;
        let sim = Arc::new(server::Sim::with_send_buffer(
            quiet_spec(300, 512),
            Some(4096),
        ));
        let addr = start(Arc::clone(&sim)).await;
        read_all(send(addr, "/robots.txt", None).await).await;
        tokio::time::sleep(Duration::from_millis(350)).await;

        // 512 KiB body, small buffers, nobody reading: it cannot be delivered yet.
        let stalled = send(addr, "/", Some(4096)).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        read_all(send(addr, "/p/0", None).await).await;
        assert_eq!(
            sim.referee.report().violations.concurrent,
            1,
            "{:?}",
            sim.referee.report().details
        );

        // Wait out the delay after the overlapping request, then drain the stalled body. Its
        // response ends only when the last byte is acknowledged, so a request 100 ms after
        // that is too soon (and would not be if the gap were measured from earlier).
        tokio::time::sleep(Duration::from_millis(350)).await;
        let _ = read_all(stalled).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        read_all(send(addr, "/p/0", None).await).await;
        let r = sim.referee.report();
        assert_eq!(r.violations.too_soon, 1, "{:?}", r.details);
    }

    /// The end of a response is the client's acknowledgement of the last byte, not the
    /// server's last write. The client reads all but the final part of the body, pauses (the
    /// server has finished writing, but the tail is still queued and unacknowledged), then
    /// reads the rest and quickly sends another request. That request is too soon.
    #[tokio::test]
    async fn the_end_is_the_acknowledgement_not_the_last_write() {
        use std::sync::Arc;
        use tokio::io::AsyncReadExt;
        let sim = Arc::new(server::Sim::with_send_buffer(
            quiet_spec(300, 512),
            Some(4096),
        ));
        let addr = start(Arc::clone(&sim)).await;
        read_all(send(addr, "/robots.txt", None).await).await;
        tokio::time::sleep(Duration::from_millis(350)).await;

        let mut slow = send(addr, "/", Some(4096)).await;
        let total = 512 * 1024;
        let mut got = 0usize;
        let mut buf = vec![0u8; 1024];
        // Leave about 12 KiB unread: more than the client's receive buffer, less than the
        // receive and send buffers together, so the server's writes are done but not all acked.
        while got + 12 * 1024 < total {
            got += slow.read(&mut buf).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(600)).await;
        let _ = read_all(slow).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        read_all(send(addr, "/p/0", None).await).await;
        let r = sim.referee.report();
        assert_eq!(r.violations.too_soon, 1, "{:?}", r.details);
    }

    /// A robots.txt the client abandons halfway does not give it permission to fetch pages.
    #[tokio::test]
    async fn an_abandoned_robots_txt_is_not_usable() {
        use std::sync::Arc;
        // A robots.txt far larger than the socket buffers, so it cannot be delivered while the
        // client is not reading.
        let mut s = quiet_spec(100, 1);
        s.disallow_hosts_pct = 100.0;
        s.robots_padding_kib = 512;
        let sim = Arc::new(server::Sim::with_send_buffer(s, Some(4096)));
        let addr = start(Arc::clone(&sim)).await;
        let abandoned = send(addr, "/robots.txt", Some(4096)).await;
        // Let the server fill the buffers and block, then hang up without reading.
        tokio::time::sleep(Duration::from_millis(200)).await;
        drop(abandoned);
        tokio::time::sleep(Duration::from_millis(300)).await;
        read_all(send(addr, "/", None).await).await;
        let r = sim.referee.report();
        assert_eq!(
            r.aborted, 1,
            "the robots.txt was not delivered: {:?}",
            r.details
        );
        assert_eq!(r.violations.before_robots, 1, "{:?}", r.details);
    }

    /// The happy path: a polite client gets no violations and full bodies.
    #[tokio::test]
    async fn a_polite_client_has_no_violations() {
        use std::sync::Arc;
        let sim = Arc::new(server::Sim::new(quiet_spec(150, 30)));
        let addr = start(Arc::clone(&sim)).await;
        for path in ["/robots.txt", "/", "/p/0", "/p/0"] {
            let body = read_all(send(addr, path, None).await).await;
            assert!(body.starts_with(b"HTTP/1.1 200 OK"), "{path}");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        let r = sim.referee.report();
        assert_eq!(
            (
                r.violations.concurrent,
                r.violations.too_soon,
                r.violations.before_robots,
                r.aborted
            ),
            (0, 0, 0, 0),
            "{:?}",
            r.details
        );
        assert_eq!(r.status_200, 4);
    }

    /// A request that is too soon only if the previous response ended late in its window is
    /// uncertain, not a violation; one that is too soon even from the earliest end is proven.
    #[test]
    fn end_windows_separate_proven_from_uncertain() {
        let s = WebSpec {
            delay_ms: 100,
            ..spec()
        };
        let r = Referee::new(1);
        let ok = Response {
            status: 200,
            headers: vec![],
            body: Body::Empty,
            latency: Duration::ZERO,
        };
        let t0 = Instant::now();
        let ms = Duration::from_millis;
        r.start(&s, 0, "/robots.txt", t0);
        r.end(0, "/robots.txt", &ok, 0, t0, t0 + ms(5), true);
        r.start(&s, 0, "/", t0 + ms(103)); // 98..103 ms after the end: uncertain
        r.end(0, "/", &ok, 0, t0 + ms(103), t0 + ms(103), true);
        r.start(&s, 0, "/p/0", t0 + ms(190)); // 87 ms: proven too soon
        let rep = r.report();
        assert_eq!(rep.violations.too_soon, 1, "{:?}", rep.details);
        assert_eq!(rep.uncertain.too_soon, 1);
        assert!((rep.end_window_ms_max - 5.0).abs() < 0.01);
    }

    #[test]
    fn a_page_after_a_failed_robots_txt_is_a_violation() {
        let s = WebSpec {
            delay_ms: 100,
            ..spec()
        };
        let r = Referee::new(1);
        let failed = Response {
            status: 503,
            headers: vec![],
            body: Body::Empty,
            latency: Duration::ZERO,
        };
        let t0 = Instant::now();
        r.start(&s, 0, "/robots.txt", t0);
        r.end(0, "/robots.txt", &failed, 0, t0, t0, true);
        r.start(&s, 0, "/", t0 + Duration::from_millis(500));
        assert_eq!(r.report().violations.before_robots, 1);
    }
}

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
    last_end: Option<Instant>,
    last_path: String,
    last_status: u16,
    /// robots.txt has been served with 200, so the crawler knows its Crawl-delay.
    rules_served: bool,
    not_before: Option<Instant>,
    robots_seen: bool,
    attempts: HashMap<String, u32>,
    requests: u64,
    min_gap_slack_ms: Option<i64>,
}

#[derive(Debug, Default, Serialize)]
pub struct Violations {
    /// A request arrived while another request to the same host was in flight.
    pub concurrent: u64,
    /// A request arrived sooner than the required delay after the previous response ended.
    pub too_soon: u64,
    /// A request arrived before a Retry-After the host sent had expired.
    pub retry_after: u64,
    /// A page was requested before the host's robots.txt.
    pub before_robots: u64,
    /// A path disallowed by robots.txt was requested.
    pub disallowed: u64,
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub requests: u64,
    pub robots_requests: u64,
    pub status_200: u64,
    pub bytes_sent: u64,
    pub unknown_host: u64,
    pub hosts_contacted: u64,
    pub violations: Violations,
    /// Smallest (gap - required delay) seen on any host, in ms. Negative means too soon.
    pub min_gap_slack_ms: Option<i64>,
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
    concurrent: AtomicU64,
    too_soon: AtomicU64,
    retry_after: AtomicU64,
    before_robots: AtomicU64,
    disallowed: AtomicU64,
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
            concurrent: AtomicU64::new(0),
            too_soon: AtomicU64::new(0),
            retry_after: AtomicU64::new(0),
            before_robots: AtomicU64::new(0),
            disallowed: AtomicU64::new(0),
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
        if let Some(end) = w.last_end {
            let gap = now.saturating_duration_since(end);
            // Crawl-delay only binds once the crawler could have read it.
            let required = if w.rules_served {
                spec.required_gap(h)
            } else {
                Duration::from_millis(spec.delay_ms)
            };
            let slack = gap.as_millis() as i64 - required.as_millis() as i64;
            w.min_gap_slack_ms = Some(w.min_gap_slack_ms.map_or(slack, |m| m.min(slack)));
            if gap + TOLERANCE < required {
                self.too_soon.fetch_add(1, Ordering::Relaxed);
                self.note(format!(
                    "too_soon host={h} path={path} gap_ms={} required_ms={} previous={} status={}",
                    gap.as_millis(),
                    required.as_millis(),
                    w.last_path,
                    w.last_status
                ));
            }
        }
        if w.not_before.is_some_and(|nb| now + TOLERANCE < nb) {
            self.retry_after.fetch_add(1, Ordering::Relaxed);
            self.note(format!("retry_after host={h} path={path}"));
        }
        if path == "/robots.txt" {
            w.robots_seen = true;
            self.robots_requests.fetch_add(1, Ordering::Relaxed);
        } else {
            if !w.robots_seen {
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

    /// Records the end of a response (after its last byte was handed to the socket).
    pub fn end(&self, h: u32, response: &Response, bytes: usize, now: Instant) {
        let mut w = self.hosts[h as usize].lock().unwrap();
        w.in_flight -= 1;
        w.last_end = Some(now);
        w.last_status = response.status;
        if w.last_path == "/robots.txt" && response.status == 200 {
            w.rules_served = true;
        }
        if response.status == 429 {
            let secs = response
                .headers
                .iter()
                .find(|(k, _)| *k == "retry-after")
                .and_then(|(_, v)| v.parse().ok());
            if let Some(secs) = secs {
                w.not_before = Some(now + Duration::from_secs(secs));
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
        r.end(0, &ok, 0, t0);
        r.end(0, &ok, 0, t0);
        r.start(&s, 0, "/p/2", t0 + Duration::from_millis(50)); // too soon
        r.end(0, &ok, 0, t0 + Duration::from_millis(50));
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
        r.end(h, &failed, 0, t0);
        // Only the base delay applies: the crawler has not seen the Crawl-delay yet.
        r.start(&s, h, "/robots.txt", t0 + Duration::from_millis(150));
        r.end(h, &ok, 0, t0 + Duration::from_millis(150));
        assert_eq!(r.report().violations.too_soon, 0);
        // Now the Crawl-delay applies.
        r.start(&s, h, "/", t0 + Duration::from_millis(300));
        assert_eq!(r.report().violations.too_soon, 1);
    }
}

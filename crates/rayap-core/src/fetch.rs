//! HTTP fetching. Redirects are never followed automatically: the target is returned to the
//! crawler, which queues it so it passes the same robots.txt and politeness checks as any
//! other request.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use reqwest::header::{CONTENT_LENGTH, CONTENT_TYPE, ETAG, LAST_MODIFIED, LOCATION, RETRY_AFTER};
use reqwest::{Client, Response, redirect};
use url::Url;

use crate::robots::MAX_ROBOTS_BYTES;
use crate::store::NewRecord;
use crate::{Config, Result, company, html, jobs, render, signing, tiktok, urlnorm};

/// How a fetched body is read, decided by the source's kind.
#[derive(Debug, Clone)]
pub enum Extractor {
    /// Title, visible-text fingerprint and links; `product_token` scopes `<meta name=...>`.
    Page { product_token: String },
    /// TikTok post and comment pages into records.
    Tiktok { max_comments: u32 },
    /// A company website: homepage, then its well-known pages.
    Company { home: Url, product_token: String },
    /// A job board feed, marking each job for `country`.
    Jobs {
        country: Arc<jobs::Country>,
        max_jobs: u32,
    },
}

pub struct Clients {
    http: Client,
    /// Present when a browser was found; `None` makes rendered sources fail loudly.
    browser: Option<render::Browser>,
    /// Present when `[signing]` is configured: every HTTP request carries Web Bot Auth headers.
    signer: Option<Arc<signing::Signer>>,
}

impl Clients {
    pub fn new(config: &Config, resolver: Option<Arc<dyn reqwest::dns::Resolve>>) -> Result<Self> {
        let mut builder = Client::builder();
        if let Some(resolver) = resolver {
            builder = builder.dns_resolver(resolver);
        }
        let http = builder
            .user_agent(config.user_agent())
            .timeout(config.request_timeout())
            .connect_timeout(config.request_timeout().min(Duration::from_secs(10)))
            // Every redirect hop goes back through the scheduler, so it gets the same
            // politeness and robots.txt checks as any other request.
            .redirect(redirect::Policy::none())
            .build()?;
        let browser = if config.sources.iter().any(|s| s.render) {
            render::Browser::find(
                &config.user_agent(),
                config.request_timeout().max(Duration::from_secs(30)),
            )
        } else {
            None
        };
        let signer = match &config.signing {
            Some(s) => {
                let agent =
                    Url::parse(&s.agent).map_err(|e| crate::Error::Config(e.to_string()))?;
                Some(Arc::new(signing::Signer::from_key_file(
                    std::path::Path::new(&s.key_file),
                    agent,
                )?))
            }
            None => None,
        };
        Ok(Self {
            http,
            browser,
            signer,
        })
    }

    /// A GET for `url`, signed when signing is configured.
    fn get(&self, url: &Url) -> reqwest::RequestBuilder {
        let mut request = self.http.get(url.clone());
        if let Some(signer) = &self.signer {
            request = request.headers(signer.headers(url, SystemTime::now()));
        }
        request
    }

    pub fn browser(&self) -> Option<&render::Browser> {
        self.browser.as_ref()
    }
}

#[derive(Debug)]
pub enum RobotsResult {
    /// 3xx with the resolved Location, if usable.
    Redirect(Option<Url>),
    /// Any other HTTP response. `body` is read only for 2xx (first 500 KiB).
    Response {
        status: u16,
        body: Vec<u8>,
        retry_after: Option<Duration>,
    },
    Network(String),
}

#[derive(Debug)]
pub enum PageResult {
    /// A 2xx response, already digested (parsing happens in the fetch task, off the
    /// coordinator, so it scales with cores).
    Body {
        status: u16,
        bytes: usize,
        title: Option<String>,
        /// Fingerprint of the title and visible text (HTML), of the raw body (other types), or
        /// of the extracted records' identity and text (record sources).
        hash: String,
        /// Links found on the page; followed on the source's host while depth allows.
        links: Vec<Url>,
        /// Pages that complete this one (the next comment page), fetched when this page is
        /// new or changed, or when `activity` changed.
        next: Vec<Url>,
        /// Fingerprint of what makes `next` worth refetching (a post's comment count), when
        /// the source separates that from content.
        activity: Option<String>,
        records: Vec<NewRecord>,
        etag: Option<String>,
        last_modified: Option<String>,
    },
    /// A 2xx body the extractor refuses to record as content: a challenge page, an empty
    /// application shell, or a post that no longer exists.
    Invalid {
        status: u16,
        bytes: usize,
        reason: Invalid,
    },
    Redirect {
        status: u16,
        location: Option<Url>,
    },
    /// 429 or 503: the server asked us to slow down.
    SlowDown {
        status: u16,
        retry_after: Option<Duration>,
    },
    /// Other HTTP status that has no usable body (4xx, 5xx).
    Status(u16),
    TooLarge {
        status: u16,
        limit: usize,
    },
    /// Connection, TLS, DNS or timeout failure.
    Network(String),
}

pub async fn fetch_page(
    clients: &Clients,
    url: &Url,
    max_body: usize,
    extractor: &Extractor,
    rendered: bool,
) -> PageResult {
    if rendered {
        // The browser route: it fetches, follows redirects and runs scripts itself; what
        // comes back is the DOM, digested exactly like an HTTP body with status 200.
        let Some(browser) = clients.browser() else {
            return PageResult::Network(
                "render: no Chromium or Chrome found on PATH (set RAYAP_BROWSER)".into(),
            );
        };
        return match browser.render(url).await {
            Ok(dom) if dom.len() > max_body => PageResult::TooLarge {
                status: 200,
                limit: max_body,
            },
            Ok(dom) => digest_body(url, 200, true, None, None, dom, extractor),
            Err(reason) => PageResult::Network(reason),
        };
    }
    let response = match clients.get(url).send().await {
        Ok(r) => r,
        Err(e) => return PageResult::Network(describe(&e)),
    };
    let status = response.status().as_u16();
    let headers = response.headers();
    match status {
        200..=299 => {
            let is_html = header(headers, CONTENT_TYPE.as_str())
                .is_some_and(|ct| ct.to_ascii_lowercase().contains("html"));
            let etag = header(headers, ETAG.as_str());
            let last_modified = header(headers, LAST_MODIFIED.as_str());
            match read_limited(response, max_body).await {
                Ok(Some(body)) => {
                    digest_body(url, status, is_html, etag, last_modified, body, extractor)
                }
                Ok(None) => PageResult::TooLarge {
                    status,
                    limit: max_body,
                },
                Err(e) => PageResult::Network(e),
            }
        }
        300..=399 => {
            let location =
                header(headers, LOCATION.as_str()).and_then(|l| urlnorm::normalize(&l, Some(url)));
            PageResult::Redirect { status, location }
        }
        429 | 503 => {
            let retry_after = header(headers, RETRY_AFTER.as_str())
                .and_then(|v| parse_retry_after(&v, SystemTime::now()));
            PageResult::SlowDown {
                status,
                retry_after,
            }
        }
        _ => PageResult::Status(status),
    }
}

/// Runs the source's extractor over a 2xx body.
fn digest_body(
    url: &Url,
    status: u16,
    is_html: bool,
    etag: Option<String>,
    last_modified: Option<String>,
    body: Vec<u8>,
    extractor: &Extractor,
) -> PageResult {
    match extractor {
        Extractor::Page { product_token } => {
            let digested = digest(&body, is_html, url, product_token);
            if is_html && digested.is_shell() {
                // HTML with no readable text at all: a JavaScript application
                // shell. Recording it would fingerprint an empty page as content.
                keep_invalid_body(url, &body);
                return PageResult::Invalid {
                    status,
                    bytes: body.len(),
                    reason: Invalid::Error("no visible text (JavaScript application shell)".into()),
                };
            }
            PageResult::Body {
                status,
                bytes: body.len(),
                title: digested.title,
                hash: digested.hash,
                links: digested.links,
                next: Vec::new(),
                activity: None,
                records: digested.record.into_iter().collect(),
                etag,
                last_modified,
            }
        }
        Extractor::Company {
            home,
            product_token,
        } => {
            let out = company::extract(url, home, &body, product_token);
            let text_len = out
                .records
                .first()
                .map_or(0, |r| r.data["text"].as_str().map_or(0, str::len));
            if text_len == 0 {
                keep_invalid_body(url, &body);
                return PageResult::Invalid {
                    status,
                    bytes: body.len(),
                    reason: Invalid::Error("no visible text (JavaScript application shell)".into()),
                };
            }
            PageResult::Body {
                status,
                bytes: body.len(),
                title: None,
                hash: fingerprint(company::FINGERPRINT_VERSION, &out.fingerprint),
                links: Vec::new(),
                next: out.next,
                activity: None,
                records: out.records,
                etag,
                last_modified,
            }
        }
        Extractor::Jobs { country, max_jobs } => {
            match jobs::extract(url, &body, country, *max_jobs) {
                Ok(out) => PageResult::Body {
                    status,
                    bytes: body.len(),
                    title: None,
                    hash: fingerprint(jobs::FINGERPRINT_VERSION, &out.fingerprint),
                    links: Vec::new(),
                    next: out.next,
                    activity: None,
                    records: out.records,
                    etag,
                    last_modified,
                },
                Err(reason) => {
                    keep_invalid_body(url, &body);
                    PageResult::Invalid {
                        status,
                        bytes: body.len(),
                        reason: Invalid::Error(reason),
                    }
                }
            }
        }
        Extractor::Tiktok { max_comments } => match tiktok::extract(url, &body, *max_comments) {
            Ok(out) => PageResult::Body {
                status,
                bytes: body.len(),
                title: None,
                hash: fingerprint(tiktok::FINGERPRINT_VERSION, &out.fingerprint),
                links: Vec::new(),
                next: out.next,
                activity: out.activity.map(|a| blake3::hash(&a).to_hex().to_string()),
                records: out.records,
                etag,
                last_modified,
            },
            Err(reason) => {
                keep_invalid_body(url, &body);
                PageResult::Invalid {
                    status,
                    bytes: body.len(),
                    reason,
                }
            }
        },
    }
}

pub async fn fetch_robots(clients: &Clients, url: &str) -> RobotsResult {
    let Ok(parsed) = Url::parse(url) else {
        return RobotsResult::Network(format!("invalid robots.txt URL {url}"));
    };
    let response = match clients.get(&parsed).send().await {
        Ok(r) => r,
        Err(e) => return RobotsResult::Network(describe(&e)),
    };
    let status = response.status().as_u16();
    if (300..400).contains(&status) {
        let location = header(response.headers(), LOCATION.as_str())
            .and_then(|l| urlnorm::normalize(&l, Some(&parsed)));
        return RobotsResult::Redirect(location);
    }
    let retry_after = header(response.headers(), RETRY_AFTER.as_str())
        .and_then(|v| parse_retry_after(&v, SystemTime::now()));
    let body = if (200..300).contains(&status) {
        match read_prefix(response, MAX_ROBOTS_BYTES).await {
            Ok(body) => body,
            Err(e) => return RobotsResult::Network(e),
        }
    } else {
        Vec::new()
    };
    RobotsResult::Response {
        status,
        body,
        retry_after,
    }
}

/// A 2xx body that must not be recorded as content.
#[derive(Debug, PartialEq, Eq)]
pub enum Invalid {
    /// The resource no longer exists (deleted, under review, failed audit).
    Gone(String),
    /// The host answered with a challenge instead of the page: it wants the client to slow
    /// down or prove itself. The page is unchanged as far as we know; try again later.
    Challenge(String),
    /// Empty shell, or a body this version cannot read.
    Error(String),
}

/// A stored fingerprint: the method's version, then the hash. Two fingerprints are only
/// compared when their versions agree; otherwise the page is re-based (see the store).
/// Bump a method's version whenever its bytes can change for the same page.
fn fingerprint(version: u32, bytes: &[u8]) -> String {
    format!("v{version}:{}", blake3::hash(bytes).to_hex())
}

struct Digested {
    title: Option<String>,
    hash: String,
    links: Vec<Url>,
    /// The page's visible text as a `page` record, so what changed can be read later.
    record: Option<NewRecord>,
    /// Characters of main-content text found.
    chars: usize,
}

impl Digested {
    /// An HTML document with no content text at all. A title alone does not count: a shell
    /// usually has one ("TikTok - Make Your Day", "Loading...").
    fn is_shell(&self) -> bool {
        self.chars == 0
    }
}

/// Visible text kept per page record. Enough for a grant call or a news page; a longer
/// page is cut at a character boundary and marked `truncated`.
const MAX_PAGE_TEXT: usize = 200 * 1024;

/// Title, content fingerprint, links and the text record of a fetched body.
fn digest(body: &[u8], is_html: bool, url: &Url, product_token: &str) -> Digested {
    if !is_html {
        return Digested {
            title: None,
            hash: fingerprint(1, body),
            links: Vec::new(),
            record: None,
            chars: body.len(),
        };
    }
    let text = String::from_utf8_lossy(body);
    let extract = html::extract(&text, url, product_token);
    let hash = fingerprint(html::FINGERPRINT_VERSION, &extract.fingerprint_source());
    let mut visible = extract.text;
    let truncated = visible.len() > MAX_PAGE_TEXT;
    if truncated {
        let mut cut = MAX_PAGE_TEXT;
        while !visible.is_char_boundary(cut) {
            cut -= 1;
        }
        visible.truncate(cut);
    }
    let chars = visible.chars().count();
    let record = NewRecord {
        kind: "page".into(),
        key: url.to_string(),
        data: serde_json::json!({
            "url": url.as_str(),
            "title": extract.title,
            "text": visible,
            "chars": chars,
            "truncated": truncated,
            // Whether `text` is the page's marked main region or everything outside chrome.
            "main_region": extract.has_main,
        }),
    };
    Digested {
        title: extract.title,
        hash,
        links: extract.links,
        record: Some(record),
        chars,
    }
}

/// With `RAYAP_KEEP_INVALID=<dir>`, bodies the extractor refused are written there for
/// inspection (what did the site actually send?). Off by default.
fn keep_invalid_body(url: &Url, body: &[u8]) {
    let Ok(dir) = std::env::var("RAYAP_KEEP_INVALID") else {
        return;
    };
    let name = format!(
        "{}-{}.body",
        unix_now_ms(),
        blake3::hash(url.as_str().as_bytes())
            .to_hex()
            .get(..12)
            .unwrap_or("")
    );
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(std::path::Path::new(&dir).join(name), body);
}

fn unix_now_ms() -> u128 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
}

fn header(headers: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
}

/// Reads the whole body, or returns `None` as soon as it exceeds `limit` bytes.
async fn read_limited(
    mut response: Response,
    limit: usize,
) -> std::result::Result<Option<Vec<u8>>, String> {
    let declared =
        header(response.headers(), CONTENT_LENGTH.as_str()).and_then(|v| v.parse::<usize>().ok());
    if declared.is_some_and(|len| len > limit) {
        return Ok(None);
    }
    let mut body = Vec::with_capacity(declared.unwrap_or(0).min(limit));
    while let Some(chunk) = response.chunk().await.map_err(|e| describe(&e))? {
        if body.len() + chunk.len() > limit {
            return Ok(None);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Some(body))
}

/// Reads at most `limit` bytes and drops the rest.
async fn read_prefix(mut response: Response, limit: usize) -> std::result::Result<Vec<u8>, String> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|e| describe(&e))? {
        let room = limit - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(room)]);
        if body.len() == limit {
            break;
        }
    }
    Ok(body)
}

/// `Retry-After` is either delay-seconds or an HTTP-date (RFC 9110 §10.2.3).
pub fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    if let Ok(secs) = value.parse::<u64>() {
        // Duration holds any u64 of seconds; callers cap the hold before adding it to a clock.
        return Some(Duration::from_secs(secs));
    }
    let at = httpdate::parse_http_date(value).ok()?;
    Some(at.duration_since(now).unwrap_or(Duration::ZERO))
}

fn describe(e: &reqwest::Error) -> String {
    let kind = if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connect"
    } else if e.is_body() || e.is_decode() {
        "body"
    } else {
        "request"
    };
    // The innermost source carries the useful detail (DNS, TLS, reset).
    let mut source: &dyn std::error::Error = e;
    while let Some(next) = source.source() {
        source = next;
    }
    format!("{kind}: {source}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_seconds_and_dates() {
        let now = httpdate::parse_http_date("Fri, 06 Nov 2026 10:00:00 GMT").unwrap();
        assert_eq!(
            parse_retry_after("120", now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            parse_retry_after("Fri, 06 Nov 2026 10:01:30 GMT", now),
            Some(Duration::from_secs(90))
        );
        assert_eq!(
            parse_retry_after("Fri, 06 Nov 2026 09:00:00 GMT", now),
            Some(Duration::ZERO),
            "past date"
        );
        assert_eq!(parse_retry_after("soon", now), None);
    }
}

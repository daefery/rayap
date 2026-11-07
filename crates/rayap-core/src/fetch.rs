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
use crate::{Config, Result, html, tiktok, urlnorm};

/// How a fetched body is read, decided by the source's kind.
#[derive(Debug, Clone)]
pub enum Extractor {
    /// Title, visible-text fingerprint and links; `product_token` scopes `<meta name=...>`.
    Page { product_token: String },
    /// TikTok post and comment pages into records.
    Tiktok { max_comments: u32 },
}

pub struct Clients {
    http: Client,
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
        Ok(Self { http })
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
        /// Pages that complete this one (the next comment page); always followed.
        next: Vec<Url>,
        records: Vec<NewRecord>,
        etag: Option<String>,
        last_modified: Option<String>,
    },
    /// A 2xx body the extractor refuses to record as content: a challenge page, an empty
    /// application shell, or a post that no longer exists.
    Invalid {
        status: u16,
        bytes: usize,
        reason: tiktok::Invalid,
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
) -> PageResult {
    let response = match clients.http.get(url.clone()).send().await {
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
                Ok(Some(body)) => match extractor {
                    Extractor::Page { product_token } => {
                        let (title, hash, links) = digest(&body, is_html, url, product_token);
                        PageResult::Body {
                            status,
                            bytes: body.len(),
                            title,
                            hash,
                            links,
                            next: Vec::new(),
                            records: Vec::new(),
                            etag,
                            last_modified,
                        }
                    }
                    Extractor::Tiktok { max_comments } => {
                        match tiktok::extract(url, &body, *max_comments) {
                            Ok(out) => PageResult::Body {
                                status,
                                bytes: body.len(),
                                title: None,
                                hash: blake3::hash(&out.fingerprint).to_hex().to_string(),
                                links: Vec::new(),
                                next: out.next,
                                records: out.records,
                                etag,
                                last_modified,
                            },
                            Err(reason) => PageResult::Invalid {
                                status,
                                bytes: body.len(),
                                reason,
                            },
                        }
                    }
                },
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

pub async fn fetch_robots(clients: &Clients, url: &str) -> RobotsResult {
    let Ok(parsed) = Url::parse(url) else {
        return RobotsResult::Network(format!("invalid robots.txt URL {url}"));
    };
    let response = match clients.http.get(parsed.clone()).send().await {
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

/// Title, content fingerprint and links of a fetched body.
fn digest(
    body: &[u8],
    is_html: bool,
    url: &Url,
    product_token: &str,
) -> (Option<String>, String, Vec<Url>) {
    if is_html {
        let text = String::from_utf8_lossy(body);
        let extract = html::extract(&text, url, product_token);
        let hash = blake3::hash(&extract.fingerprint_source())
            .to_hex()
            .to_string();
        (extract.title, hash, extract.links)
    } else {
        (None, blake3::hash(body).to_hex().to_string(), Vec::new())
    }
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

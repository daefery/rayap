//! Crawl configuration, read from a TOML file.

use std::collections::HashSet;
use std::time::Duration;

use serde::Deserialize;

use crate::{Error, Result, urlnorm};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub crawler: CrawlerConfig,
    /// Sign every request (Web Bot Auth, RFC 9421 + Ed25519) so sites can verify it is rayap.
    #[serde(default)]
    pub signing: Option<SigningConfig>,
    #[serde(rename = "source", default)]
    pub sources: Vec<Source>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SigningConfig {
    /// File holding the Ed25519 seed: 32 raw bytes or their base64. `rayap key new` makes one.
    pub key_file: String,
    /// https URL whose `/.well-known/http-message-signatures-directory` serves the public key
    /// (`rayap key directory` prints the document). Sent as `Signature-Agent`.
    pub agent: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CrawlerConfig {
    /// URL or email that site owners can use to reach whoever runs the crawler.
    /// It is sent in every User-Agent. Required.
    pub contact: String,
    /// robots.txt product token (RFC 9309: letters, `_` and `-` only).
    #[serde(default = "default_product_token")]
    pub product_token: String,
    /// Requests in flight across all hosts. Each host still gets at most one.
    #[serde(default = "default_max_concurrency")]
    pub max_concurrency: usize,
    /// Minimum wait between the end of one request and the start of the next on the same host.
    #[serde(default = "default_per_host_delay_ms")]
    pub per_host_delay_ms: u64,
    /// Upper bound applied to a site's robots.txt `Crawl-delay`.
    #[serde(default = "default_max_crawl_delay_ms")]
    pub max_crawl_delay_ms: u64,
    #[serde(default = "default_request_timeout_ms")]
    pub request_timeout_ms: u64,
    /// Pages larger than this are recorded as errors instead of being stored.
    #[serde(default = "default_max_body_bytes")]
    pub max_body_bytes: usize,
    /// Attempts per URL before a retryable failure becomes an `error` event.
    #[serde(default = "default_max_attempts")]
    pub max_attempts: u32,
    /// Longest a run waits for a held host (Retry-After, backoff). Beyond this the run ends
    /// unfinished; the hold is stored and the next run resumes the remaining pages.
    #[serde(default = "default_max_wait_ms")]
    pub max_wait_ms: u64,
    /// Wait after a network error or 5xx before retrying, multiplied by the attempt number.
    #[serde(default = "default_retry_backoff_ms")]
    pub retry_backoff_ms: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub name: String,
    pub url: String,
    /// How many links away from `url` to follow, on the same host. 0 = only `url` itself.
    #[serde(default)]
    pub max_depth: u32,
    /// How the pages of this source are read. Default: fingerprint the visible text.
    #[serde(default)]
    pub kind: SourceKind,
    /// For `kind = "tiktok"`: comments to collect per post (rounded up to whole API pages).
    #[serde(default = "default_max_comments")]
    pub max_comments: u32,
    /// Fetch this source's pages with a headless browser so their JavaScript runs first.
    /// For pages that are an empty shell over plain HTTP. Needs Chromium or Chrome on PATH
    /// (or `RAYAP_BROWSER`); without one the pages are reported as errors.
    #[serde(default)]
    pub render: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    /// Any web page: title, visible-text fingerprint, links.
    #[default]
    Page,
    /// A public TikTok post (`/@handle/video/<id>`): `post` and `comment` records.
    Tiktok,
    /// An organisation's website: a `company` record from the homepage and a `company_page`
    /// record for each well-known page it links (about, careers, contact, blog, products).
    Company,
}

fn default_max_comments() -> u32 {
    60
}

fn default_product_token() -> String {
    "rayap".into()
}
fn default_max_concurrency() -> usize {
    64
}
fn default_per_host_delay_ms() -> u64 {
    2_000
}
fn default_max_crawl_delay_ms() -> u64 {
    60_000
}
fn default_request_timeout_ms() -> u64 {
    30_000
}
fn default_max_body_bytes() -> usize {
    10 * 1024 * 1024
}
fn default_max_attempts() -> u32 {
    3
}
fn default_max_wait_ms() -> u64 {
    10 * 60 * 1000
}
fn default_retry_backoff_ms() -> u64 {
    30_000
}

impl Config {
    pub fn from_toml(text: &str) -> Result<Self> {
        let config: Config = toml::from_str(text).map_err(|e| Error::Config(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        let c = &self.crawler;
        if c.contact.trim().is_empty() {
            return Err(Error::Config(
                "crawler.contact must be a URL or email that site owners can use to reach you"
                    .into(),
            ));
        }
        let token_ok = !c.product_token.is_empty()
            && c.product_token
                .chars()
                .all(|ch| ch.is_ascii_alphabetic() || ch == '_' || ch == '-');
        if !token_ok {
            return Err(Error::Config(
                "crawler.product_token may contain only letters, '_' and '-' (RFC 9309)".into(),
            ));
        }
        if c.max_concurrency == 0 || c.max_attempts == 0 {
            return Err(Error::Config(
                "crawler.max_concurrency and crawler.max_attempts must be at least 1".into(),
            ));
        }
        if self.sources.is_empty() {
            return Err(Error::Config("add at least one [[source]]".into()));
        }
        if let Some(s) = &self.signing {
            match url::Url::parse(&s.agent) {
                Ok(u) if u.scheme() == "https" => {}
                _ => {
                    return Err(Error::Config("signing.agent must be an https URL".into()));
                }
            }
        }
        let mut names = HashSet::new();
        for source in &self.sources {
            if !names.insert(source.name.as_str()) {
                return Err(Error::Config(format!(
                    "duplicate source name '{}'",
                    source.name
                )));
            }
            let Some(url) = urlnorm::normalize(&source.url, None) else {
                return Err(Error::Config(format!(
                    "source '{}': '{}' is not an http(s) URL",
                    source.name, source.url
                )));
            };
            if source.kind == SourceKind::Tiktok
                && !matches!(
                    crate::tiktok::classify(&url),
                    Some(crate::tiktok::Page::Post { .. })
                )
            {
                return Err(Error::Config(format!(
                    "source '{}': a tiktok source must be a post URL like https://www.tiktok.com/@handle/video/<id>",
                    source.name
                )));
            }
        }
        Ok(())
    }

    pub fn user_agent(&self) -> String {
        format!(
            "Mozilla/5.0 (compatible; {}/{}; +{})",
            self.crawler.product_token,
            env!("CARGO_PKG_VERSION"),
            self.crawler.contact.trim()
        )
    }

    pub fn per_host_delay(&self) -> Duration {
        Duration::from_millis(self.crawler.per_host_delay_ms)
    }

    pub fn max_crawl_delay(&self) -> Duration {
        Duration::from_millis(self.crawler.max_crawl_delay_ms)
    }

    pub fn max_wait(&self) -> Duration {
        Duration::from_millis(self.crawler.max_wait_ms)
    }

    pub fn retry_backoff(&self) -> Duration {
        Duration::from_millis(self.crawler.retry_backoff_ms)
    }

    pub fn request_timeout(&self) -> Duration {
        Duration::from_millis(self.crawler.request_timeout_ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        [crawler]
        contact = "https://example.org/bot"
        [[source]]
        name = "ngs"
        url = "https://www.nationalgeographic.org/explore/grants-investments"
    "#;

    #[test]
    fn minimal_config_gets_polite_defaults() {
        let cfg = Config::from_toml(MINIMAL).unwrap();
        assert_eq!(cfg.crawler.per_host_delay_ms, 2_000);
        assert_eq!(cfg.sources[0].max_depth, 0);
        assert_eq!(
            cfg.user_agent(),
            format!(
                "Mozilla/5.0 (compatible; rayap/{}; +https://example.org/bot)",
                env!("CARGO_PKG_VERSION")
            )
        );
    }

    #[test]
    fn contact_is_required() {
        let text = MINIMAL.replace("https://example.org/bot", " ");
        assert!(matches!(Config::from_toml(&text), Err(Error::Config(m)) if m.contains("contact")));
    }

    #[test]
    fn product_token_must_follow_rfc_9309() {
        let text = MINIMAL.replace("[crawler]", "[crawler]\nproduct_token = \"rayap2\"");
        assert!(
            matches!(Config::from_toml(&text), Err(Error::Config(m)) if m.contains("product_token"))
        );
    }

    #[test]
    fn rejects_non_http_sources_and_duplicates() {
        let bad_url = MINIMAL.replace(
            "https://www.nationalgeographic.org/explore/grants-investments",
            "ftp://x",
        );
        assert!(Config::from_toml(&bad_url).is_err());
        let dup = format!("{MINIMAL}\n[[source]]\nname = \"ngs\"\nurl = \"https://example.org\"\n");
        assert!(
            matches!(Config::from_toml(&dup), Err(Error::Config(m)) if m.contains("duplicate"))
        );
    }

    #[test]
    fn tiktok_sources_must_be_post_urls() {
        let ok = MINIMAL.replace(
            "https://www.nationalgeographic.org/explore/grants-investments",
            "https://www.tiktok.com/@tiktok/video/7106594312292453675\"\nkind = \"tiktok",
        );
        let cfg = Config::from_toml(&ok).unwrap();
        assert_eq!(
            (cfg.sources[0].kind, cfg.sources[0].max_comments),
            (SourceKind::Tiktok, 60)
        );
        let bad = MINIMAL.replace(
            "url = \"https://www.nationalgeographic.org/explore/grants-investments\"",
            "url = \"https://www.tiktok.com/@tiktok\"\nkind = \"tiktok\"",
        );
        assert!(matches!(Config::from_toml(&bad), Err(Error::Config(m)) if m.contains("post URL")));
        assert_eq!(
            Config::from_toml(MINIMAL).unwrap().sources[0].kind,
            SourceKind::Page
        );
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let text = MINIMAL.replace("[crawler]", "[crawler]\nper_host_delay = 5");
        assert!(Config::from_toml(&text).is_err());
    }
}

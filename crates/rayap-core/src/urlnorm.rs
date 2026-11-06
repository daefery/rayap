//! URL identity: one canonical string per page, so the same page is never queued twice.

use url::Url;

/// Parses `raw` (relative to `base` when given) into the URL rayap stores and fetches.
/// Keeps only http(s), drops the fragment and any userinfo, and keeps the query as is
/// (query parameters often select different content).
pub fn normalize(raw: &str, base: Option<&Url>) -> Option<Url> {
    let raw = raw.trim();
    let mut url = match base {
        Some(base) => base.join(raw).ok()?,
        None => Url::parse(raw).ok()?,
    };
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return None;
    }
    url.set_fragment(None);
    // Both setters only fail for URLs that cannot have userinfo, which http(s) URLs can.
    let _ = url.set_username("");
    let _ = url.set_password(None);
    Some(url)
}

/// Politeness key: one queue and one rate limit per host name, across schemes and ports.
pub fn host(url: &Url) -> String {
    url.host_str().unwrap_or_default().to_string()
}

/// robots.txt key: RFC 9309 scopes robots.txt to scheme, host and port.
pub fn origin(url: &Url) -> String {
    url.origin().ascii_serialization()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(raw: &str) -> Option<String> {
        normalize(raw, None).map(String::from)
    }

    #[test]
    fn drops_fragment_and_userinfo_keeps_query() {
        assert_eq!(
            norm("https://user:pw@Example.org/a?b=1#top").unwrap(),
            "https://example.org/a?b=1"
        );
    }

    #[test]
    fn resolves_relative_links() {
        let base = Url::parse("https://example.org/grants/list.html").unwrap();
        assert_eq!(
            normalize("../about", Some(&base)).unwrap().as_str(),
            "https://example.org/about"
        );
        assert_eq!(
            normalize("call-2026", Some(&base)).unwrap().as_str(),
            "https://example.org/grants/call-2026"
        );
    }

    #[test]
    fn rejects_non_web_schemes() {
        for raw in [
            "mailto:a@b.org",
            "javascript:void(0)",
            "ftp://example.org/",
            "data:text/plain,hi",
        ] {
            assert_eq!(norm(raw), None, "{raw}");
        }
    }

    #[test]
    fn default_ports_collapse() {
        assert_eq!(
            norm("https://example.org:443/").unwrap(),
            "https://example.org/"
        );
    }

    #[test]
    fn host_and_origin_keys() {
        let url = Url::parse("http://example.org:8080/x").unwrap();
        assert_eq!(host(&url), "example.org");
        assert_eq!(origin(&url), "http://example.org:8080");
    }
}

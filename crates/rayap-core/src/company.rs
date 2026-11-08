//! Company source: one organisation's public website into one `company` record.
//!
//! The homepage is read for what sites publish about themselves: meta description, Open
//! Graph tags, a JSON-LD `Organization` block, and the standard navigation targets (about,
//! careers, contact, blog, press) plus official social profiles. Those well-known pages are
//! then fetched as continuation pages, each contributing a `company_page` record with its
//! main text, so "what does this company do and is it hiring" can be read and watched.
//!
//! Public, organisation-level facts only. Nothing here looks for people, personal email
//! addresses or phone numbers, and nothing follows links off the company's own host except
//! to record (not fetch) its social profile URLs.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;
use url::Url;

use crate::store::NewRecord;
use crate::{html, urlnorm};

/// Version of the company fingerprints. History: 1 (8 Nov 2026).
pub const FINGERPRINT_VERSION: u32 = 1;

/// Well-known pages, by the role they play. Matched against link paths, case-insensitive.
const WELL_KNOWN: &[(&str, &[&str])] = &[
    (
        "about",
        &["/about", "/about-us", "/company", "/who-we-are", "/mission"],
    ),
    (
        "careers",
        &[
            "/careers",
            "/jobs",
            "/join",
            "/join-us",
            "/hiring",
            "/work-with-us",
            "/vacancies",
        ],
    ),
    ("contact", &["/contact", "/contact-us", "/get-in-touch"]),
    (
        "blog",
        &[
            "/blog",
            "/news",
            "/press",
            "/newsroom",
            "/updates",
            "/insights",
        ],
    ),
    (
        "products",
        &[
            "/products",
            "/product",
            "/services",
            "/solutions",
            "/pricing",
        ],
    ),
];

const SOCIAL_HOSTS: &[(&str, &str)] = &[
    ("linkedin.com", "linkedin"),
    ("x.com", "x"),
    ("twitter.com", "x"),
    ("github.com", "github"),
    ("youtube.com", "youtube"),
    ("instagram.com", "instagram"),
    ("facebook.com", "facebook"),
    ("tiktok.com", "tiktok"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page<'a> {
    /// The source URL: the homepage.
    Home,
    /// One of the well-known pages, with its role.
    WellKnown(&'a str),
}

#[derive(Debug, PartialEq, Eq)]
pub struct Extracted {
    pub records: Vec<NewRecord>,
    /// Well-known pages to fetch (homepage only).
    pub next: Vec<Url>,
    pub fingerprint: Vec<u8>,
}

#[derive(Serialize, Default)]
struct Company<'a> {
    platform: &'static str,
    url: String,
    host: String,
    name: Option<String>,
    legal_name: Option<&'a str>,
    description: Option<String>,
    /// Open Graph / JSON-LD type when present (`website`, `Organization`, `WebAPI`).
    kind: Option<String>,
    logo: Option<&'a str>,
    /// Role -> URL on the company's own host.
    pages: BTreeMap<String, String>,
    /// Network -> profile URL, as linked from the homepage.
    social: BTreeMap<String, String>,
    /// Main-content text of the homepage, for search and change reading.
    text: String,
}

/// Which kind of company page `url` is, given the source's homepage `home`. The role of a
/// well-known page is read from its path, so no state from the homepage visit is needed.
pub fn classify(url: &Url, home: &Url) -> Page<'static> {
    // The homepage is the configured URL, or the root of the same organisation's host after
    // a redirect (www.example.org -> example.org).
    let root = url.path().trim_matches('/').is_empty() && url.query().is_none();
    if url.as_str() == home.as_str() || (root && same_organisation(url, home)) {
        return Page::Home;
    }
    let path = url.path().trim_end_matches('/').to_ascii_lowercase();
    for (role, patterns) in WELL_KNOWN {
        if patterns.iter().any(|p| path == *p) {
            return Page::WellKnown(role);
        }
    }
    Page::WellKnown("page")
}

/// Extracts from a 2xx body of `url`. `home` is the source URL.
pub fn extract(url: &Url, home: &Url, body: &[u8], product_token: &str) -> Extracted {
    let text = String::from_utf8_lossy(body);
    let page = classify(url, home);
    let extract = html::extract(&text, url, product_token);
    match page {
        Page::Home => extract_home(url, &text, extract),
        Page::WellKnown(role) => {
            let record = NewRecord {
                kind: "company_page".into(),
                key: url.to_string(),
                data: serde_json::json!({
                    "platform": "web",
                    "company": urlnorm::host(home),
                    "role": role,
                    "url": url.as_str(),
                    "title": extract.title,
                    "text": extract.text,
                }),
            };
            Extracted {
                fingerprint: format!(
                    "company_page\n{role}\n{}\n{}",
                    extract.title.as_deref().unwrap_or_default(),
                    extract.text
                )
                .into_bytes(),
                records: vec![record],
                next: Vec::new(),
            }
        }
    }
}

fn extract_home(url: &Url, raw: &str, extract: html::Extract) -> Extracted {
    let meta = meta_tags(raw);
    let ld = json_ld_organization(raw);
    let host = urlnorm::host(url);
    let name = ld
        .as_ref()
        .and_then(|o| o["name"].as_str())
        .map(str::to_string)
        .or_else(|| meta.get("og:site_name").cloned())
        .or_else(|| meta.get("og:title").cloned())
        .or_else(|| extract.title.clone());
    let description = meta
        .get("description")
        .cloned()
        .or_else(|| meta.get("og:description").cloned())
        .or_else(|| {
            ld.as_ref()
                .and_then(|o| o["description"].as_str())
                .map(str::to_string)
        });
    let kind = ld
        .as_ref()
        .and_then(|o| o["@type"].as_str())
        .map(str::to_string)
        .or_else(|| meta.get("og:type").cloned());

    let mut pages = BTreeMap::new();
    let mut social = BTreeMap::new();
    for link in &extract.links {
        if link.host_str() == url.host_str() {
            let path = link.path().trim_end_matches('/').to_ascii_lowercase();
            for (role, patterns) in WELL_KNOWN {
                if patterns.iter().any(|p| path == *p) && !pages.contains_key(*role) {
                    pages.insert(role.to_string(), link.to_string());
                }
            }
        } else if let Some(h) = link.host_str() {
            let h = h.trim_start_matches("www.");
            for (domain, network) in SOCIAL_HOSTS {
                if (h == *domain || h.ends_with(&format!(".{domain}")))
                    && !social.contains_key(*network)
                    && is_profile_path(link.path())
                {
                    social.insert(network.to_string(), link.to_string());
                }
            }
        }
    }
    let company = Company {
        platform: "web",
        url: url.to_string(),
        host: host.clone(),
        name,
        legal_name: ld.as_ref().and_then(|o| o["legalName"].as_str()),
        description,
        kind,
        logo: ld
            .as_ref()
            .and_then(|o| o["logo"].as_str().or_else(|| o["logo"]["url"].as_str())),
        pages: pages.clone(),
        social,
        text: extract.text.clone(),
    };
    let next = pages.values().filter_map(|u| Url::parse(u).ok()).collect();
    Extracted {
        fingerprint: format!(
            "company\n{}\n{}\n{}",
            company.name.as_deref().unwrap_or_default(),
            company.description.as_deref().unwrap_or_default(),
            extract.text
        )
        .into_bytes(),
        records: vec![NewRecord {
            kind: "company".into(),
            key: host,
            data: serde_json::to_value(&company).expect("plain struct"),
        }],
        next,
    }
}

/// Same registrable host ignoring a `www.` prefix.
fn same_organisation(a: &Url, b: &Url) -> bool {
    let strip = |u: &Url| {
        u.host_str()
            .unwrap_or_default()
            .trim_start_matches("www.")
            .to_ascii_lowercase()
    };
    strip(a) == strip(b)
}

/// A social URL that names a profile or page, not a share/intent link.
fn is_profile_path(path: &str) -> bool {
    let p = path.trim_matches('/');
    !p.is_empty()
        && !p.starts_with("share")
        && !p.starts_with("intent")
        && !p.starts_with("sharer")
        && !p.starts_with("shareArticle")
}

/// `<meta name|property=... content=...>` pairs, lower-cased names, attribute order free.
fn meta_tags(html: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let lower = html.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find("<meta") {
        let start = from + i;
        let Some(len) = lower[start..].find('>') else {
            break;
        };
        let tag = &html[start..start + len];
        let name = attr(tag, "name").or_else(|| attr(tag, "property"));
        if let (Some(name), Some(content)) = (name, attr(tag, "content")) {
            let name = name.to_ascii_lowercase();
            if !content.trim().is_empty() {
                out.entry(name)
                    .or_insert_with(|| html::decode_entities(content.trim()));
            }
        }
        from = start + len;
    }
    out
}

fn attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let lower = tag.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(name) {
        let at = from + i;
        let before_ok = at == 0 || !lower.as_bytes()[at - 1].is_ascii_alphanumeric();
        let rest = &tag[at + name.len()..];
        let rest_trim = rest.trim_start();
        if before_ok && let Some(after_eq) = rest_trim.strip_prefix('=') {
            let v = after_eq.trim_start();
            let quote = v.chars().next()?;
            if quote == '"' || quote == '\'' {
                let inner = &v[1..];
                return inner.find(quote).map(|e| &inner[..e]);
            }
            return Some(
                v.split(|c: char| c.is_whitespace() || c == '>')
                    .next()
                    .unwrap_or(""),
            );
        }
        from = at + name.len();
    }
    None
}

/// The first JSON-LD object of type Organization (or a subtype), searching `@graph` too.
fn json_ld_organization(html: &str) -> Option<Value> {
    let lower = html.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find("application/ld+json") {
        let at = from + i;
        let Some(open) = lower[at..].find('>') else {
            break;
        };
        let body_start = at + open + 1;
        let Some(close) = lower[body_start..].find("</script>") else {
            break;
        };
        let body = &html[body_start..body_start + close];
        if let Ok(v) = serde_json::from_str::<Value>(body) {
            let candidates: Vec<Value> = match v {
                Value::Array(a) => a,
                Value::Object(ref o) if o.contains_key("@graph") => {
                    o["@graph"].as_array().cloned().unwrap_or_default()
                }
                other => vec![other],
            };
            for c in candidates {
                let ty = c["@type"].as_str().unwrap_or("").to_string();
                if ty.ends_with("Organization")
                    || ty == "Corporation"
                    || ty == "NGO"
                    || ty == "LocalBusiness"
                    || ty == "WebAPI"
                {
                    return Some(c);
                }
            }
        }
        from = body_start + close;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = r#"<!doctype html><html><head>
        <title>Exa</title>
        <meta name="description" content="Search API for AI agents that need real-time web data">
        <meta property="og:site_name" content="Exa">
        <meta property="og:type" content="website">
        <script type="application/ld+json">{"@context":"https://schema.org","@type":"Organization","name":"Exa","legalName":"Exa Labs, Inc.","url":"https://exa.ai","logo":"https://exa.ai/logo.png"}</script>
        </head><body>
        <header><nav><a href="/about">About</a><a href="/careers">Careers</a><a href="/blog/exa-3">Blog</a><a href="/contact/sales">Sales</a></nav></header>
        <main><h1>Web search for AI</h1><p>Realtime data from the web for your AI.</p></main>
        <footer><a href="https://www.linkedin.com/company/exa-ai/">LinkedIn</a><a href="https://x.com/exaailabs">X</a>
        <a href="https://x.com/intent/tweet?text=hi">share</a><a href="https://github.com/exa-labs">GitHub</a></footer>
        </body></html>"#;

    fn home() -> Url {
        Url::parse("https://exa.ai/").unwrap()
    }

    #[test]
    fn homepage_yields_a_company_record_and_well_known_pages() {
        let out = extract(&home(), &home(), HOME.as_bytes(), "rayap");
        assert_eq!(out.records.len(), 1);
        let r = &out.records[0];
        assert_eq!((r.kind.as_str(), r.key.as_str()), ("company", "exa.ai"));
        assert_eq!(r.data["name"], "Exa");
        assert_eq!(r.data["legal_name"], "Exa Labs, Inc.");
        assert_eq!(r.data["kind"], "Organization");
        assert_eq!(r.data["logo"], "https://exa.ai/logo.png");
        assert_eq!(
            r.data["description"],
            "Search API for AI agents that need real-time web data"
        );
        assert_eq!(
            r.data["text"],
            "Web search for AI Realtime data from the web for your AI."
        );
        assert_eq!(r.data["pages"]["about"], "https://exa.ai/about");
        assert_eq!(r.data["pages"]["careers"], "https://exa.ai/careers");
        assert!(
            r.data["pages"].get("blog").is_none(),
            "/blog/exa-3 is a post, not the blog index"
        );
        assert!(
            r.data["pages"].get("contact").is_none(),
            "/contact/sales is not the contact page"
        );
        assert_eq!(
            r.data["social"]["linkedin"],
            "https://www.linkedin.com/company/exa-ai/"
        );
        assert_eq!(
            r.data["social"]["x"], "https://x.com/exaailabs",
            "the share intent is skipped"
        );
        assert_eq!(r.data["social"]["github"], "https://github.com/exa-labs");
        let next: Vec<&str> = out.next.iter().map(Url::as_str).collect();
        assert_eq!(next, ["https://exa.ai/about", "https://exa.ai/careers"]);
    }

    #[test]
    fn well_known_page_yields_a_company_page_record() {
        let careers = Url::parse("https://exa.ai/careers").unwrap();
        let body = "<html><body><nav>menu</nav><main><h1>Careers</h1><p>Software Engineer, Web Crawling. Zurich.</p></main></body></html>";
        let out = extract(&careers, &home(), body.as_bytes(), "rayap");
        let r = &out.records[0];
        assert_eq!(
            (r.kind.as_str(), r.key.as_str()),
            ("company_page", "https://exa.ai/careers")
        );
        assert_eq!(r.data["role"], "careers");
        assert_eq!(r.data["company"], "exa.ai");
        assert_eq!(
            r.data["text"],
            "Careers Software Engineer, Web Crawling. Zurich."
        );
        assert!(
            out.next.is_empty(),
            "well-known pages do not fan out further"
        );
        // A new opening changes the fingerprint; a menu change does not.
        let hired = body.replace("Zurich.", "Zurich. Senior Rust Engineer.");
        assert_ne!(
            extract(&careers, &home(), hired.as_bytes(), "rayap").fingerprint,
            out.fingerprint
        );
        let menu = body.replace("menu", "menu v2");
        assert_eq!(
            extract(&careers, &home(), menu.as_bytes(), "rayap").fingerprint,
            out.fingerprint
        );
    }

    #[test]
    fn the_root_after_a_www_redirect_is_still_the_homepage() {
        let configured = Url::parse("https://www.solveeducation.org/").unwrap();
        let landed = Url::parse("https://solveeducation.org/").unwrap();
        assert_eq!(classify(&landed, &configured), Page::Home);
        assert_eq!(
            classify(
                &Url::parse("https://solveeducation.org/careers/").unwrap(),
                &configured
            ),
            Page::WellKnown("careers")
        );
        assert_eq!(
            classify(&Url::parse("https://other.org/").unwrap(), &configured),
            Page::WellKnown("page")
        );
    }

    #[test]
    fn falls_back_when_there_is_no_structured_data() {
        let plain = "<html><head><title>NLnet Foundation</title></head><body><p>We fund open internet projects.</p><a href='/contact/'>Contact</a></body></html>";
        let u = Url::parse("https://nlnet.nl/").unwrap();
        let out = extract(&u, &u, plain.as_bytes(), "rayap");
        let r = &out.records[0];
        assert_eq!(r.data["name"], "NLnet Foundation");
        assert!(r.data["description"].is_null());
        assert_eq!(r.data["pages"]["contact"], "https://nlnet.nl/contact/");
    }

    #[test]
    fn json_ld_graph_and_meta_attribute_order() {
        let html = r#"<meta content="Unlocking youth potential" property="og:description"><meta content="Solve Education!" property="og:site_name">
        <script type="application/ld+json">{"@context":"https://schema.org","@graph":[{"@type":"WebSite","name":"site"},{"@type":"NGO","name":"Solve Education","url":"https://solveeducation.org"}]}</script>"#;
        let m = meta_tags(html);
        assert_eq!(m["og:description"], "Unlocking youth potential");
        let o = json_ld_organization(html).unwrap();
        assert_eq!(o["name"], "Solve Education");
    }
}

//! Streaming HTML extraction: title, visible text fingerprint, and followable links.

use std::cell::RefCell;

use lol_html::html_content::TextType;
use lol_html::{HtmlRewriter, Settings, doc_text, element, text};
use url::Url;

use crate::urlnorm;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Extract {
    pub title: Option<String>,
    /// Body text with whitespace collapsed. Script, style, title and other non-data text is
    /// excluded, so changes to tracking code or inline CSS do not count as content changes.
    pub text: String,
    /// Absolute, normalized link targets, in document order, without duplicates.
    /// Empty when the page asks robots not to follow links.
    pub links: Vec<Url>,
}

#[derive(Default)]
struct State {
    title: String,
    title_done: bool,
    text: String,
    hrefs: Vec<String>,
    base: Option<String>,
    nofollow: bool,
}

/// Extracts from `html`, resolving links against `page_url` (or a `<base href>`).
/// `product_token` lets `<meta name="rayap" content="nofollow">` apply to this crawler only.
pub fn extract(html: &str, page_url: &Url, product_token: &str) -> Extract {
    let state = RefCell::new(State::default());
    let result = {
        let settings = Settings::new()
            .append_element_content_handler(element!("a[href]", |el| {
                let rel = el.get_attribute("rel").unwrap_or_default();
                let nofollow = rel
                    .split_ascii_whitespace()
                    .any(|r| r.eq_ignore_ascii_case("nofollow"));
                if !nofollow && let Some(href) = el.get_attribute("href") {
                    state.borrow_mut().hrefs.push(href);
                }
                Ok(())
            }))
            .append_element_content_handler(element!("base[href]", |el| {
                let mut s = state.borrow_mut();
                if s.base.is_none() {
                    s.base = el.get_attribute("href");
                }
                Ok(())
            }))
            .append_element_content_handler(element!("meta[name][content]", |el| {
                let name = el.get_attribute("name").unwrap_or_default();
                if name.eq_ignore_ascii_case("robots") || name.eq_ignore_ascii_case(product_token) {
                    let content = el
                        .get_attribute("content")
                        .unwrap_or_default()
                        .to_ascii_lowercase();
                    if content
                        .split(',')
                        .map(str::trim)
                        .any(|d| d == "nofollow" || d == "none")
                    {
                        state.borrow_mut().nofollow = true;
                    }
                }
                Ok(())
            }))
            // The first non-empty <title> is the document title; later ones belong to SVG icons.
            // (`head > title` would miss pages that omit the optional <head> tag.)
            .append_element_content_handler(text!("title", |chunk| {
                let mut s = state.borrow_mut();
                if !s.title_done {
                    s.title.push_str(chunk.as_str());
                    if chunk.last_in_text_node() && !s.title.trim().is_empty() {
                        s.title_done = true;
                    }
                }
                Ok(())
            }))
            .append_document_content_handler(doc_text!(|chunk| {
                if chunk.text_type() == TextType::Data {
                    let mut s = state.borrow_mut();
                    s.text.push_str(chunk.as_str());
                    s.text.push(' ');
                }
                Ok(())
            }));
        let mut rewriter = HtmlRewriter::new(settings, |_: &[u8]| {});
        rewriter
            .write(html.as_bytes())
            .and_then(|()| rewriter.end())
    };
    // A malformed document still yields whatever was read before the error.
    let _ = result;

    let state = state.into_inner();
    let base = state
        .base
        .as_deref()
        .and_then(|b| urlnorm::normalize(b, Some(page_url)))
        .unwrap_or_else(|| page_url.clone());
    let mut links: Vec<Url> = Vec::new();
    if !state.nofollow {
        for href in &state.hrefs {
            if let Some(url) = urlnorm::normalize(href, Some(&base))
                && !links.contains(&url)
            {
                links.push(url);
            }
        }
    }
    let title = collapse(&decode_basic_entities(&state.title));
    Extract {
        title: (!title.is_empty()).then_some(title),
        text: collapse(&decode_basic_entities(&state.text)),
        links,
    }
}

impl Extract {
    /// The bytes whose fingerprint decides whether a page changed: title and body text.
    pub fn fingerprint_source(&self) -> Vec<u8> {
        format!(
            "{}\n{}",
            self.title.as_deref().unwrap_or_default(),
            self.text
        )
        .into_bytes()
    }
}

/// Collapses whitespace runs to one space and trims, in one pass.
fn collapse(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut pending_space = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(ch);
        }
    }
    out
}

/// Decodes the entities that commonly appear in titles and text, so that `&amp;` and `&`
/// fingerprint the same. Other entities are kept as written. One pass; `&amp;lt;` becomes
/// `&lt;` (decoded once), as a browser shows it.
fn decode_basic_entities(s: &str) -> String {
    const ENTITIES: [(&str, char); 6] = [
        ("&nbsp;", ' '),
        ("&lt;", '<'),
        ("&gt;", '>'),
        ("&quot;", '"'),
        ("&#39;", '\''),
        ("&amp;", '&'),
    ];
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        match ENTITIES.iter().find(|(e, _)| rest.starts_with(e)) {
            Some((e, ch)) => {
                out.push(*ch);
                rest = &rest[e.len()..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page() -> Url {
        Url::parse("https://example.org/grants/index.html").unwrap()
    }

    #[test]
    fn extracts_title_text_and_links() {
        let html = r#"<html><head><title> Open calls &amp; deadlines </title>
            <style>body { color: red }</style><script>track("x")</script></head>
            <body><h1>Grants</h1><p>Deadline:   1 Dec 2026</p>
            <a href="call-1">Call 1</a> <a href="/about#team">About</a>
            <a href="call-1">again</a> <a href="mailto:x@example.org">mail</a></body></html>"#;
        let e = extract(html, &page(), "rayap");
        assert_eq!(e.title.as_deref(), Some("Open calls & deadlines"));
        assert_eq!(
            e.text,
            "Grants Deadline: 1 Dec 2026 Call 1 About again mail"
        );
        let links: Vec<&str> = e.links.iter().map(Url::as_str).collect();
        assert_eq!(
            links,
            [
                "https://example.org/grants/call-1",
                "https://example.org/about"
            ]
        );
    }

    #[test]
    fn svg_titles_are_not_the_page_title() {
        let html = r#"<html><head><title>Grants</title></head><body><svg><title>Icons/Close</title></svg></body></html>"#;
        assert_eq!(
            extract(html, &page(), "rayap").title.as_deref(),
            Some("Grants")
        );
    }

    #[test]
    fn title_without_head_tag() {
        let html = r#"<title>Grants</title><p>x</p><svg><title>Icon</title></svg>"#;
        assert_eq!(
            extract(html, &page(), "rayap").title.as_deref(),
            Some("Grants")
        );
    }

    #[test]
    fn script_and_style_changes_do_not_change_text() {
        let a = extract(
            "<p>Same</p><script>var t=1</script><style>p{}</style>",
            &page(),
            "rayap",
        );
        let b = extract(
            "<p>Same</p><script>var t=2</script><style>p{x:y}</style>",
            &page(),
            "rayap",
        );
        assert_eq!(a.text, b.text);
    }

    #[test]
    fn honours_nofollow() {
        let rel = extract(
            r#"<a rel="external nofollow" href="/x">x</a><a href="/y">y</a>"#,
            &page(),
            "rayap",
        );
        assert_eq!(rel.links.len(), 1);
        assert_eq!(rel.links[0].path(), "/y");

        let meta = extract(
            r#"<meta name="ROBOTS" content="noindex, nofollow"><a href="/y">y</a>"#,
            &page(),
            "rayap",
        );
        assert!(meta.links.is_empty());

        let ours = extract(
            r#"<meta name="rayap" content="none"><a href="/y">y</a>"#,
            &page(),
            "rayap",
        );
        assert!(ours.links.is_empty());
        let other = extract(
            r#"<meta name="otherbot" content="none"><a href="/y">y</a>"#,
            &page(),
            "rayap",
        );
        assert_eq!(other.links.len(), 1);
    }

    #[test]
    fn base_href_changes_link_resolution() {
        let e = extract(
            r#"<base href="https://cdn.example.org/root/"><a href="doc">d</a>"#,
            &page(),
            "rayap",
        );
        assert_eq!(e.links[0].as_str(), "https://cdn.example.org/root/doc");
    }

    #[test]
    fn text_cleanup() {
        assert_eq!(collapse("  a \n\t b  c "), "a b c");
        assert_eq!(collapse(""), "");
        assert_eq!(
            decode_basic_entities("A &amp; B &lt;x&gt; &quot;q&quot; it&#39;s&nbsp;ok"),
            "A & B <x> \"q\" it's ok"
        );
        assert_eq!(
            decode_basic_entities("&amp;lt; &copy; & &"),
            "&lt; &copy; & &"
        );
        assert_eq!(decode_basic_entities("café"), "café");
    }

    #[test]
    fn malformed_html_still_extracts() {
        let e = extract("<p>unclosed <a href='/z'>z", &page(), "rayap");
        assert_eq!(e.text, "unclosed z");
        assert_eq!(e.links.len(), 1);
    }
}

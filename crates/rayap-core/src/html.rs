//! Streaming HTML extraction: title, main-content text, and followable links.
//!
//! Text inside site chrome (`nav`, `header`, `footer`, `aside`, the ARIA roles that mean
//! the same, inline `svg`, and the usual screen-reader-only classes) is left out, and when a page marks its main region (`main`, `article`,
//! `role="main"`) only that region's text counts. A page with no such marks keeps all its
//! text. This makes the fingerprint about the content, not the menu, and keeps the stored
//! page record readable.

use std::cell::RefCell;
use std::rc::Rc;

use lol_html::html_content::TextType;
use lol_html::{EndTagHandler, HtmlRewriter, Settings, doc_text, element, text};
use url::Url;

use crate::urlnorm;

/// Version of the page fingerprint (`fingerprint_source`). History:
/// 1 title + visible text, named entities decoded;
/// 2 numeric character references decoded too (7 Nov 2026);
/// 3 chrome (nav, header, footer, aside) excluded; main/article preferred (8 Nov 2026).
pub const FINGERPRINT_VERSION: u32 = 3;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Extract {
    pub title: Option<String>,
    /// Main-content text with whitespace collapsed. Script, style, title, site chrome and
    /// other non-content text is excluded, so a menu item or a tracking snippet changing does
    /// not count as a content change.
    pub text: String,
    /// True when the page marked a main region and `text` is only that region.
    pub has_main: bool,
    /// Absolute, normalized link targets, in document order, without duplicates.
    /// Empty when the page asks robots not to follow links.
    pub links: Vec<Url>,
}

#[derive(Default, Clone)]
struct State {
    title: String,
    title_done: bool,
    /// All content text, whitespace already collapsed, used when the page has no main region.
    text: Collapsed,
    /// Text inside the main region only, collapsed.
    main_text: Collapsed,
    /// How many chrome elements enclose the current position (text there is dropped).
    chrome_depth: u32,
    /// How many main-region elements enclose the current position.
    main_depth: u32,
    saw_main: bool,
    hrefs: Vec<String>,
    base: Option<String>,
    nofollow: bool,
}

const CHROME: &str = "nav, header, footer, aside, [role=navigation], [role=banner], \
    [role=contentinfo], [role=complementary], [role=search], [role=menu], [role=menubar], \
    [role=dialog], [aria-hidden=true], noscript, svg, template, \
    .screen-reader-text, .sr-only, .visually-hidden";
const MAIN: &str = "main, article, [role=main]";

/// Extracts from `html`, resolving links against `page_url` (or a `<base href>`).
/// `product_token` lets `<meta name="rayap" content="nofollow">` apply to this crawler only.
pub fn extract(html: &str, page_url: &Url, product_token: &str) -> Extract {
    let state = Rc::new(RefCell::new(State::default()));
    let result = {
        let settings = Settings::new()
            // Element handlers fire at the start tag; the end-tag handler closes the region.
            .append_element_content_handler(element!(CHROME, |el| {
                state.borrow_mut().chrome_depth += 1;
                let state_for_void = Rc::clone(&state);
                let state = Rc::clone(&state);
                // A void element has no end tag; `on_end_tag` errors for it and we ignore that:
                // its depth increment is undone right away.
                let handler: EndTagHandler<'static> = Box::new(move |_| {
                    let mut s = state.borrow_mut();
                    s.chrome_depth = s.chrome_depth.saturating_sub(1);
                    Ok(())
                });
                let on_end = el.on_end_tag(handler);
                if on_end.is_err() {
                    let mut s = state_for_void.borrow_mut();
                    s.chrome_depth = s.chrome_depth.saturating_sub(1);
                }
                Ok(())
            }))
            .append_element_content_handler(element!(MAIN, |el| {
                {
                    let mut s = state.borrow_mut();
                    s.main_depth += 1;
                    s.saw_main = true;
                }
                let state_for_void = Rc::clone(&state);
                let state = Rc::clone(&state);
                let handler: EndTagHandler<'static> = Box::new(move |_| {
                    let mut s = state.borrow_mut();
                    s.main_depth = s.main_depth.saturating_sub(1);
                    Ok(())
                });
                let on_end = el.on_end_tag(handler);
                if on_end.is_err() {
                    let mut s = state_for_void.borrow_mut();
                    s.main_depth = s.main_depth.saturating_sub(1);
                }
                Ok(())
            }))
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
                // A <title> inside chrome (an inline SVG's accessible name, a dialog) is not
                // the document title, wherever in the document it appears.
                if !s.title_done && s.chrome_depth == 0 {
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
                    if s.chrome_depth > 0 {
                        return Ok(());
                    }
                    s.text.push(chunk.as_str());
                    if s.main_depth > 0 {
                        s.main_text.push(chunk.as_str());
                    }
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

    let state = Rc::try_unwrap(state)
        .map(RefCell::into_inner)
        .unwrap_or_else(|rc| rc.borrow().clone());
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
    // A marked main region wins, unless it turned out empty (a shell with an empty <main>).
    // Entities are decoded after collapsing: a decoded entity may be a space, which is why
    // the result is trimmed, but nothing else about whitespace changes.
    let main_text = decode_basic_entities(state.main_text.finish());
    let has_main = state.saw_main && !main_text.is_empty();
    let text = if has_main {
        main_text
    } else {
        decode_basic_entities(state.text.finish())
    };
    Extract {
        title: (!title.is_empty()).then_some(title),
        text,
        has_main,
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

/// Text with whitespace runs collapsed to one space as it is appended, so a page is scanned
/// once. ASCII whitespace is handled bytewise (the common case); other Unicode whitespace is
/// handled when a chunk contains non-ASCII bytes.
#[derive(Default, Clone)]
struct Collapsed {
    out: String,
    pending_space: bool,
}

impl Collapsed {
    fn push(&mut self, chunk: &str) {
        if chunk.is_ascii() {
            for &b in chunk.as_bytes() {
                if b.is_ascii_whitespace() {
                    self.pending_space = !self.out.is_empty();
                } else {
                    if self.pending_space {
                        self.out.push(' ');
                        self.pending_space = false;
                    }
                    self.out.push(b as char);
                }
            }
        } else {
            for ch in chunk.chars() {
                if ch.is_whitespace() {
                    self.pending_space = !self.out.is_empty();
                } else {
                    if self.pending_space {
                        self.out.push(' ');
                        self.pending_space = false;
                    }
                    self.out.push(ch);
                }
            }
        }
        // A chunk boundary separates words even without whitespace in the markup (`<b>a</b>b`
        // renders as "ab", but adjacent block elements render apart); one space is the safe,
        // stable choice and matches the previous behaviour of appending ' ' per chunk.
        self.pending_space = !self.out.is_empty();
    }

    fn finish(&self) -> &str {
        &self.out
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
/// fingerprint the same: the named ones below and any numeric reference (`&#8211;`,
/// `&#x2F;`). Other named entities are kept as written. One pass; `&amp;lt;` becomes `&lt;`
/// (decoded once), as a browser shows it.
pub fn decode_entities(s: &str) -> String {
    decode_basic_entities(s)
}

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
            None => match numeric_entity(rest) {
                Some((ch, len)) => {
                    out.push(ch);
                    rest = &rest[len..];
                }
                None => {
                    out.push('&');
                    rest = &rest[1..];
                }
            },
        }
    }
    out.push_str(rest);
    out
}

/// `&#NNN;` or `&#xHHH;` at the start of `s`: the character and the reference's length.
fn numeric_entity(s: &str) -> Option<(char, usize)> {
    let body = s.strip_prefix("&#")?;
    let end = body.find(';')?;
    if end == 0 || end > 8 {
        return None;
    }
    let digits = &body[..end];
    let code = match digits.strip_prefix(['x', 'X']) {
        Some(hex) => u32::from_str_radix(hex, 16).ok()?,
        None => digits.parse::<u32>().ok()?,
    };
    let ch = char::from_u32(code).filter(|c| *c != '\0')?;
    Some((ch, 2 + end + 1))
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
            decode_basic_entities("Grants &#8211; Alpha &#x2F; Omega &#39;x&#39; &bogus; &#;"),
            "Grants – Alpha / Omega 'x' &bogus; &#;"
        );
        assert_eq!(
            e.text,
            "Grants Deadline: 1 Dec 2026 Call 1 About again mail"
        );
        assert!(!e.has_main);
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
    fn chrome_is_excluded_and_a_main_region_wins() {
        let html = r#"<html><body>
            <header><a href="/">Site</a> <nav><a href="/a">A</a> <a href="/b">B</a></nav></header>
            <div role="banner">Cookie notice</div>
            <main><h1>Open call</h1><p>Deadline 3 Jan 2027</p></main>
            <aside>Related: <a href="/c">C</a></aside>
            <footer>© 2026 Example</footer></body></html>"#;
        let e = extract(html, &page(), "rayap");
        assert!(e.has_main);
        assert_eq!(e.text, "Open call Deadline 3 Jan 2027");
        // Links are still collected everywhere: the crawl frontier is a different concern.
        assert_eq!(e.links.len(), 4);

        // Chrome changes do not change the fingerprint; content changes do.
        let menu_changed = html.replace(">B</a>", ">B2</a>");
        assert_eq!(
            extract(&menu_changed, &page(), "rayap").fingerprint_source(),
            e.fingerprint_source()
        );
        let deadline_moved = html.replace("3 Jan", "10 Jan");
        assert_ne!(
            extract(&deadline_moved, &page(), "rayap").fingerprint_source(),
            e.fingerprint_source()
        );
    }

    #[test]
    fn without_a_main_region_all_non_chrome_text_counts() {
        let html = "<html><body><nav>Menu</nav><div><p>Body one</p><p>Body two</p></div><footer>f</footer></body></html>";
        let e = extract(html, &page(), "rayap");
        assert!(!e.has_main);
        assert_eq!(e.text, "Body one Body two");
        // An empty <main> (application shell) falls back to the rest, so the validator's
        // "no visible text" signal is not hidden by an empty region.
        let shell = "<html><body><nav>Menu</nav><main id=app></main><p>Loading</p></body></html>";
        let e = extract(shell, &page(), "rayap");
        assert!(!e.has_main);
        assert_eq!(e.text, "Loading");
        // Nested chrome inside main is still chrome; article inside main is content.
        let nested = "<main><nav>Jump to</nav><article><h2>Call</h2><p>Text</p></article><aside>Share</aside></main>";
        assert_eq!(extract(nested, &page(), "rayap").text, "Call Text");
        // Inline SVG (its <style> and <title> are not page text) and screen-reader labels.
        let svg = r#"<div><svg><style>.st0{fill:#fff}</style><title>Logo</title></svg><span class="screen-reader-text">Close Search</span><p>Real</p></div>"#;
        assert_eq!(extract(svg, &page(), "rayap").text, "Real");
    }

    #[test]
    fn svg_titles_are_not_the_page_title() {
        let html = r#"<html><head><title>Grants</title></head><body><svg><title>Icons/Close</title></svg></body></html>"#;
        assert_eq!(
            extract(html, &page(), "rayap").title.as_deref(),
            Some("Grants")
        );
        // An SVG icon in the header, before the document title (seen on nationalgeographic.org).
        let icon_first = r#"<html><body><header><svg><title>Icons/Content/map</title></svg></header><title>AI Policy</title><main>Text</main></body></html>"#;
        assert_eq!(
            extract(icon_first, &page(), "rayap").title.as_deref(),
            Some("AI Policy")
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

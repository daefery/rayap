//! robots.txt per RFC 9309 (https://www.rfc-editor.org/rfc/rfc9309.html).
//!
//! - Group selection: every group whose user-agent matches our product token
//!   (case-insensitive) is combined; if none matches, every `*` group is combined; otherwise
//!   there are no rules.
//! - Matching: `*` matches any sequence, a trailing `$` anchors the end, other patterns match
//!   as prefixes of the path and query. The rule with the most octets wins; on a tie, allow
//!   wins. Patterns and paths are compared after percent-encoding normalization, so
//!   `/%70rivate` and `/private` are the same path.
//! - Empty `Allow:`/`Disallow:` values are ignored. `/robots.txt` itself is always allowed.
//! - Fetch outcome: 2xx = parse; 4xx = "unavailable", allow all; 5xx or network failure =
//!   "unreachable", disallow all. Only the first 500 KiB is parsed, and rules are reused for
//!   at most 24 hours.
//!
//! Failures never widen access: an over-long disallow pattern is shortened (which can only
//! make it match more), and an over-long allow pattern is dropped.

use std::time::Duration;

/// RFC 9309 §2.5: parse at least 500 KiB.
pub const MAX_ROBOTS_BYTES: usize = 500 * 1024;
/// RFC 9309 §2.4: do not use a cached robots.txt for more than 24 hours.
pub const MAX_CACHE_AGE: Duration = Duration::from_secs(24 * 60 * 60);
/// Patterns longer than this are shortened (disallow) or dropped (allow), which bounds the
/// cost of matching a URL against hostile input.
const MAX_PATTERN_BYTES: usize = 2048;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    allow: bool,
    /// Normalized pattern without the trailing `$`.
    pattern: Vec<u8>,
    anchored: bool,
    /// Octets of the pattern as written (after normalization); decides precedence.
    priority: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Rules {
    AllowAll,
    DisallowAll {
        reason: String,
    },
    Parsed {
        rules: Vec<Rule>,
        crawl_delay: Option<f64>,
    },
}

/// What a robots.txt fetch produced.
#[derive(Debug, Clone, PartialEq)]
pub enum Fetched {
    Rules(Rules),
    /// Server or network error: treat as disallowed, and ask again later.
    Unreachable {
        reason: String,
    },
}

impl Rules {
    /// `path_and_query` is the part of the URL from the path on, e.g. `/a/b?c=d`.
    pub fn allowed(&self, path_and_query: &str) -> bool {
        match self {
            Rules::AllowAll => true,
            Rules::DisallowAll { .. } => false,
            Rules::Parsed { rules, .. } => {
                if path_and_query == "/robots.txt" {
                    return true;
                }
                let path = normalize(path_and_query.as_bytes());
                let mut best: Option<&Rule> = None;
                for rule in rules
                    .iter()
                    .filter(|r| matches(&r.pattern, r.anchored, &path))
                {
                    let better = match best {
                        None => true,
                        Some(b) => {
                            rule.priority > b.priority
                                || (rule.priority == b.priority && rule.allow && !b.allow)
                        }
                    };
                    if better {
                        best = Some(rule);
                    }
                }
                best.is_none_or(|r| r.allow)
            }
        }
    }

    /// The `Crawl-delay` for our group, clamped to `max`. Not part of RFC 9309, but widely used.
    pub fn crawl_delay(&self, max: Duration) -> Option<Duration> {
        match self {
            Rules::Parsed {
                crawl_delay: Some(secs),
                ..
            } => Some(Duration::try_from_secs_f64(*secs).unwrap_or(max).min(max)),
            _ => None,
        }
    }

    pub fn block_reason(&self) -> String {
        match self {
            Rules::DisallowAll { reason } => reason.clone(),
            _ => "disallowed by robots.txt".into(),
        }
    }
}

/// Interprets the final (non-redirect) response to a robots.txt request.
pub fn interpret(status: u16, body: &[u8], product_token: &str) -> Fetched {
    match status {
        200..=299 => Fetched::Rules(parse(
            &body[..body.len().min(MAX_ROBOTS_BYTES)],
            product_token,
        )),
        // 429 asks us to slow down, so it is treated like a server error (as Google does).
        429 => Fetched::Unreachable {
            reason: "robots.txt returned HTTP 429".into(),
        },
        400..=499 => Fetched::Rules(Rules::AllowAll),
        _ => Fetched::Unreachable {
            reason: format!("robots.txt returned HTTP {status}"),
        },
    }
}

#[derive(Default)]
struct Group {
    agents: Vec<String>,
    rules: Vec<Rule>,
    crawl_delay: Option<f64>,
}

pub fn parse(body: &[u8], product_token: &str) -> Rules {
    let body = body.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(body);
    let mut groups: Vec<Group> = Vec::new();
    let mut in_agent_lines = false;

    for raw in body.split(|b| *b == b'\n' || *b == b'\r') {
        let line = match raw.iter().position(|b| *b == b'#') {
            Some(i) => &raw[..i],
            None => raw,
        };
        let Some(colon) = line.iter().position(|b| *b == b':') else {
            continue;
        };
        let key = String::from_utf8_lossy(line[..colon].trim_ascii()).to_ascii_lowercase();
        let value = line[colon + 1..].trim_ascii();

        match key.as_str() {
            "user-agent" => {
                if !in_agent_lines {
                    groups.push(Group::default());
                    in_agent_lines = true;
                }
                let agent = agent_token(value);
                if let Some(group) = groups.last_mut() {
                    group.agents.push(agent);
                }
            }
            "allow" | "disallow" => {
                in_agent_lines = false;
                // Rules before the first user-agent line belong to no group (RFC 9309 §2.1).
                let Some(group) = groups.last_mut() else {
                    continue;
                };
                if let Some(rule) = make_rule(key == "allow", value) {
                    group.rules.push(rule);
                }
            }
            "crawl-delay" => {
                in_agent_lines = false;
                let Some(group) = groups.last_mut() else {
                    continue;
                };
                let parsed = std::str::from_utf8(value)
                    .ok()
                    .and_then(|v| v.trim().parse::<f64>().ok());
                if let Some(secs) = parsed.filter(|s| s.is_finite() && *s >= 0.0) {
                    group.crawl_delay.get_or_insert(secs);
                }
            }
            // Sitemap and unknown keys do not end a group of user-agent lines.
            _ => {}
        }
    }

    let token = product_token.to_ascii_lowercase();
    let ours: Vec<&Group> = groups
        .iter()
        .filter(|g| g.agents.iter().any(|a| *a == token))
        .collect();
    let selected = if ours.is_empty() {
        groups
            .iter()
            .filter(|g| g.agents.iter().any(|a| a == "*"))
            .collect()
    } else {
        ours
    };
    Rules::Parsed {
        rules: selected
            .iter()
            .flat_map(|g| g.rules.iter().cloned())
            .collect(),
        crawl_delay: selected.iter().find_map(|g| g.crawl_delay),
    }
}

/// The product-token part of a user-agent value: leading letters, `_` and `-`, lowercased
/// (`Googlebot/2.1` -> `googlebot`). `*` stays `*`.
fn agent_token(value: &[u8]) -> String {
    if value.first() == Some(&b'*') {
        return "*".into();
    }
    let end = value
        .iter()
        .position(|b| !(b.is_ascii_alphabetic() || *b == b'_' || *b == b'-'))
        .unwrap_or(value.len());
    String::from_utf8_lossy(&value[..end]).to_ascii_lowercase()
}

fn make_rule(allow: bool, value: &[u8]) -> Option<Rule> {
    if value.is_empty() {
        return None;
    }
    let mut pattern = normalize(value);
    let anchored = pattern.last() == Some(&b'$');
    if anchored {
        pattern.pop();
    }
    // Collapse runs of `*`; they match the same strings and only cost time.
    pattern.dedup_by(|a, b| *a == b'*' && *b == b'*');
    let priority = pattern.len() + usize::from(anchored);
    if pattern.len() > MAX_PATTERN_BYTES {
        if allow {
            return None;
        }
        // A prefix of a pattern, unanchored, matches everything the pattern matches.
        pattern.truncate(MAX_PATTERN_BYTES);
        return Some(Rule {
            allow,
            pattern,
            anchored: false,
            priority,
        });
    }
    Some(Rule {
        allow,
        pattern,
        anchored,
        priority,
    })
}

/// Percent-encoding normalization used for both patterns and paths: `%XX` for an
/// unreserved character is decoded, other `%XX` get uppercase hex, and bytes outside
/// printable ASCII are encoded.
fn normalize(input: &[u8]) -> Vec<u8> {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        let b = input[i];
        if b == b'%'
            && let (Some(h), Some(l)) = (
                input.get(i + 1).and_then(hex),
                input.get(i + 2).and_then(hex),
            )
        {
            let decoded = h * 16 + l;
            if decoded.is_ascii_alphanumeric() || b"-._~".contains(&decoded) {
                out.push(decoded);
            } else {
                out.extend_from_slice(&[b'%', HEX[usize::from(h)], HEX[usize::from(l)]]);
            }
            i += 3;
            continue;
        }
        if b.is_ascii_graphic() {
            out.push(b);
        } else {
            out.extend_from_slice(&[b'%', HEX[usize::from(b >> 4)], HEX[usize::from(b & 15)]]);
        }
        i += 1;
    }
    out
}

fn hex(b: &u8) -> Option<u8> {
    (*b as char).to_digit(16).map(|d| d as u8)
}

/// Does `pattern` (with `*` wildcards) match `path`? Unanchored patterns match prefixes.
/// Greedy matching with one backtrack point: O(len(path) * len(pattern)) at worst.
fn matches(pattern: &[u8], anchored: bool, path: &[u8]) -> bool {
    let (mut p, mut t) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    loop {
        if p == pattern.len() && (!anchored || t == path.len()) {
            return true;
        }
        if p < pattern.len() && pattern[p] == b'*' {
            star = Some((p, t));
            p += 1;
            continue;
        }
        if p < pattern.len() && t < path.len() && pattern[p] == path[t] {
            p += 1;
            t += 1;
            continue;
        }
        match star {
            Some((sp, st)) if st < path.len() => {
                star = Some((sp, st + 1));
                p = sp + 1;
                t = st + 1;
            }
            _ => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(body: &str) -> Rules {
        parse(body.as_bytes(), "rayap")
    }

    #[test]
    fn our_group_wins_over_star() {
        let r = rules("User-agent: *\nDisallow: /\n\nUser-agent: rayap\nDisallow: /private\n");
        assert!(r.allowed("/grants"));
        assert!(!r.allowed("/private/x"));
    }

    #[test]
    fn product_token_match_ignores_case_and_version() {
        assert!(!rules("User-agent: RAYAP\nDisallow: /x\n").allowed("/x"));
        assert!(!rules("User-agent: rayap/1.0\nDisallow: /x\n").allowed("/x"));
        assert!(
            rules("User-agent: rayapbot\nDisallow: /x\n").allowed("/x"),
            "different token"
        );
    }

    #[test]
    fn matching_groups_are_combined() {
        let r = rules(
            "User-agent: rayap\nDisallow: /a\n\nUser-agent: other\nUser-agent: rayap\nDisallow: /b\n",
        );
        assert!(!r.allowed("/a") && !r.allowed("/b"));
        assert!(r.allowed("/c"));
    }

    #[test]
    fn falls_back_to_star_then_to_nothing() {
        let r = rules("User-agent: otherbot\nDisallow: /\n\nUser-agent: *\nDisallow: /tmp\n");
        assert!(r.allowed("/grants") && !r.allowed("/tmp/a"));
        assert!(rules("User-agent: otherbot\nDisallow: /\n").allowed("/anything"));
    }

    #[test]
    fn longest_match_wins_and_allow_wins_ties() {
        let r = rules("User-agent: *\nDisallow: /grants\nAllow: /grants/open\n");
        assert!(r.allowed("/grants/open/2026"));
        assert!(!r.allowed("/grants/closed"));
        let tie = rules("User-agent: *\nDisallow: /page\nAllow: /page\n");
        assert!(tie.allowed("/page"));
    }

    #[test]
    fn wildcards_and_end_anchor() {
        let r = rules("User-agent: *\nDisallow: /*.pdf$\nDisallow: /search*q=\n");
        assert!(!r.allowed("/files/call.pdf"));
        assert!(r.allowed("/files/call.pdf?download=1"));
        assert!(!r.allowed("/search?lang=en&q=grants"));
        assert!(r.allowed("/search?lang=en"));
    }

    #[test]
    fn empty_disallow_does_not_override_other_rules() {
        let r = rules("User-agent: *\nDisallow: /\nDisallow:\n");
        assert!(!r.allowed("/a"));
        assert!(rules("User-agent: *\nDisallow:\n").allowed("/a"));
    }

    #[test]
    fn percent_encoding_is_normalized() {
        let r =
            rules("User-agent: *\nDisallow: /private\nDisallow: /%7euser\nDisallow: /caf%C3%A9\n");
        assert!(!r.allowed("/%70rivate"));
        assert!(!r.allowed("/~user/x"));
        assert!(!r.allowed("/caf%c3%a9"));
        let utf8 = rules("User-agent: *\nDisallow: /café\n");
        assert!(!utf8.allowed("/caf%C3%A9"));
    }

    #[test]
    fn robots_txt_is_always_allowed() {
        assert!(rules("User-agent: *\nDisallow: /\n").allowed("/robots.txt"));
    }

    #[test]
    fn rules_before_any_user_agent_are_ignored() {
        assert!(rules("Disallow: /\nUser-agent: *\nAllow: /x\n").allowed("/a"));
    }

    #[test]
    fn sitemap_lines_do_not_split_a_group() {
        let r =
            rules("User-agent: a\nSitemap: https://x.org/s.xml\nUser-agent: rayap\nDisallow: /x\n");
        assert!(!r.allowed("/x"));
    }

    #[test]
    fn comments_bom_and_crlf() {
        let r = rules("\u{feff}User-agent: * # everyone\r\nDisallow: /tmp # temp\r\n");
        assert!(!r.allowed("/tmp/1"));
        assert!(r.allowed("/tm"), "/tm is not under /tmp");
    }

    #[test]
    fn over_long_patterns_fail_closed() {
        let long = format!("/{}$", "a*".repeat(5000));
        let r = rules(&format!("User-agent: *\nDisallow: /\nDisallow: {long}\n"));
        assert!(!r.allowed("/z"), "a bad rule must not unblock the site");
        let allow = rules(&format!("User-agent: *\nDisallow: /\nAllow: {long}\n"));
        assert!(!allow.allowed("/aaaa"), "over-long allow is dropped");
        let dis = rules(&format!(
            "User-agent: *\nDisallow: /x{}\n",
            "y".repeat(3000)
        ));
        assert!(
            !dis.allowed(&format!("/x{}", "y".repeat(3000))),
            "shortened disallow still blocks"
        );
    }

    #[test]
    fn hostile_patterns_stay_fast() {
        let r = rules(&format!(
            "User-agent: *\nDisallow: /{}b\n",
            "a*".repeat(1000)
        ));
        let path = format!("/{}", "a".repeat(2000));
        let start = std::time::Instant::now();
        assert!(r.allowed(&path));
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn crawl_delay_is_read_and_clamped() {
        let max = Duration::from_secs(60);
        assert_eq!(
            rules("User-agent: rayap\nCrawl-delay: 5\n").crawl_delay(max),
            Some(Duration::from_secs(5))
        );
        assert_eq!(
            rules("User-agent: *\nCrawl-delay: 0.5\n").crawl_delay(max),
            Some(Duration::from_millis(500))
        );
        assert_eq!(
            rules("User-agent: *\nCrawl-delay: 1e20\n").crawl_delay(max),
            Some(max)
        );
        assert_eq!(
            rules("User-agent: *\nCrawl-delay: -3\n").crawl_delay(max),
            None
        );
        assert_eq!(
            rules("User-agent: *\nCrawl-delay: soon\n").crawl_delay(max),
            None
        );
    }

    #[test]
    fn fetch_status_interpretation() {
        for status in [401, 403, 404, 410] {
            assert_eq!(
                interpret(status, b"", "rayap"),
                Fetched::Rules(Rules::AllowAll),
                "{status}"
            );
        }
        for status in [429, 500, 503] {
            assert!(
                matches!(interpret(status, b"", "rayap"), Fetched::Unreachable { .. }),
                "{status}"
            );
        }
    }

    #[test]
    fn only_first_500_kib_is_parsed() {
        let mut body = "User-agent: *\nDisallow: /a\n".to_string();
        body.push_str(&"# padding\n".repeat(MAX_ROBOTS_BYTES / 10));
        body.push_str("Disallow: /b\n");
        let Fetched::Rules(r) = interpret(200, body.as_bytes(), "rayap") else {
            panic!()
        };
        assert!(!r.allowed("/a"));
        assert!(r.allowed("/b"));
    }
}

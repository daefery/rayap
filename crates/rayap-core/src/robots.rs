//! robots.txt handling per RFC 9309.
//!
//! - 2xx: parse and follow the group for our product token, else the `*` group.
//! - 4xx ("unavailable"): the crawler may access everything.
//! - 5xx or network failure ("unreachable"): assume complete disallow.
//! - Rules are cached for at most 24 hours, and only the first 500 KiB is parsed.

use std::time::Duration;

use texting_robots::Robot;

/// RFC 9309 §2.5: parse at least 500 KiB.
pub const MAX_ROBOTS_BYTES: usize = 500 * 1024;
/// RFC 9309 §2.4: do not use a cached robots.txt for more than 24 hours.
pub const MAX_CACHE_AGE: Duration = Duration::from_secs(24 * 60 * 60);

pub enum Rules {
    AllowAll,
    DisallowAll { reason: String },
    Parsed(Robot),
}

/// What a robots.txt fetch produced.
pub enum Fetched {
    Rules(Rules),
    /// Server or network error: retry later, and treat as disallowed meanwhile.
    Unreachable {
        reason: String,
    },
}

impl Rules {
    pub fn allowed(&self, url: &str) -> bool {
        match self {
            Rules::AllowAll => true,
            Rules::DisallowAll { .. } => false,
            Rules::Parsed(robot) => robot.allowed(url),
        }
    }

    pub fn crawl_delay(&self) -> Option<Duration> {
        match self {
            Rules::Parsed(robot) => robot
                .delay
                .filter(|d| d.is_finite() && *d > 0.0)
                .map(Duration::from_secs_f32),
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

/// Interprets the final response to a robots.txt request (after redirects).
pub fn interpret(status: u16, body: &[u8], product_token: &str) -> Fetched {
    match status {
        200..=299 => {
            let body = &body[..body.len().min(MAX_ROBOTS_BYTES)];
            match Robot::new(product_token, body) {
                Ok(robot) => Fetched::Rules(Rules::Parsed(robot)),
                // RFC 9309 §2.2: unparseable lines are ignored, so a file with nothing usable
                // carries no rules.
                Err(_) => Fetched::Rules(Rules::AllowAll),
            }
        }
        400..=499 => Fetched::Rules(Rules::AllowAll),
        _ => Fetched::Unreachable {
            reason: format!("robots.txt returned HTTP {status}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(status: u16, body: &str) -> Rules {
        match interpret(status, body.as_bytes(), "rayap") {
            Fetched::Rules(r) => r,
            Fetched::Unreachable { .. } => panic!("unexpected unreachable"),
        }
    }

    #[test]
    fn our_group_wins_over_star() {
        let r = rules(
            200,
            "User-agent: *\nDisallow: /\n\nUser-agent: rayap\nDisallow: /private\n",
        );
        assert!(r.allowed("https://example.org/grants"));
        assert!(!r.allowed("https://example.org/private/x"));
    }

    #[test]
    fn product_token_match_is_case_insensitive() {
        let r = rules(200, "User-agent: RAYAP\nDisallow: /x\n");
        assert!(!r.allowed("https://example.org/x"));
    }

    #[test]
    fn falls_back_to_star_group() {
        let r = rules(
            200,
            "User-agent: otherbot\nDisallow: /\n\nUser-agent: *\nDisallow: /tmp\n",
        );
        assert!(r.allowed("https://example.org/grants"));
        assert!(!r.allowed("https://example.org/tmp/a"));
    }

    #[test]
    fn longest_match_allow_beats_disallow() {
        let r = rules(
            200,
            "User-agent: *\nDisallow: /grants\nAllow: /grants/open\n",
        );
        assert!(r.allowed("https://example.org/grants/open/2026"));
        assert!(!r.allowed("https://example.org/grants/closed"));
    }

    #[test]
    fn crawl_delay_is_read() {
        let r = rules(200, "User-agent: rayap\nCrawl-delay: 5\nDisallow:\n");
        assert_eq!(r.crawl_delay(), Some(Duration::from_secs(5)));
    }

    #[test]
    fn client_errors_mean_allow_all() {
        for status in [401, 403, 404, 410] {
            assert!(
                rules(status, "").allowed("https://example.org/anything"),
                "{status}"
            );
        }
    }

    #[test]
    fn server_errors_mean_unreachable() {
        for status in [500, 503] {
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
        let r = rules(200, &body);
        assert!(!r.allowed("https://example.org/a"));
        assert!(r.allowed("https://example.org/b"));
    }
}

//! Remote-job boards with public JSON feeds, into `job` records, each marked with whether
//! someone living in a given country can apply.
//!
//! Boards (all three publish these feeds for redistribution with attribution and a link back
//! to the listing; both are in every record):
//! - Himalayas: `https://himalayas.app/jobs/api?limit=N[&cursor=...]`, cursor-paged,
//!   per-job country list and allowed UTC offsets;
//! - Remotive: `https://remotive.com/api/remote-jobs`, one page, free-text
//!   `candidate_required_location`. Supported, but its robots.txt disallows `/api/*`, so a
//!   crawl of it is `blocked` unless the board changes that;
//! - Remote OK: `https://remoteok.com/api`, one page, free-text `location`.
//!
//! Eligibility is a rule, not a guess: a job is `eligible_from` the configured country when
//! the board names that country, says Worldwide/Anywhere/Global or the country's region, or
//! sets no country and either no timezone window or one that includes the country's offsets.
//! Everything else is `false`, with the board's own text kept so the reader can disagree.

use serde::Serialize;
use serde_json::Value;
use url::Url;

use crate::store::NewRecord;

/// Version of the job fingerprint. History: 1 (8 Nov 2026).
pub const FINGERPRINT_VERSION: u32 = 1;
/// Himalayas page size; the feed caps at 20 whatever is asked.
const HIMALAYAS_PAGE: u32 = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Board {
    Himalayas,
    Remotive,
    RemoteOk,
}

impl Board {
    pub fn name(self) -> &'static str {
        match self {
            Board::Himalayas => "Himalayas",
            Board::Remotive => "Remotive",
            Board::RemoteOk => "Remote OK",
        }
    }
}

/// The country a reader lives in, and what counts as "includes it".
#[derive(Debug, Clone)]
pub struct Country {
    pub name: String,
    /// Other words a board might use for the same place or its region.
    pub aliases: Vec<String>,
    /// UTC offsets in hours the country spans.
    pub offsets: Vec<f64>,
}

impl Country {
    pub fn indonesia() -> Self {
        Self {
            name: "Indonesia".into(),
            aliases: [
                "indonesia",
                "southeast asia",
                "south east asia",
                "sea",
                "asia",
                "apac",
                "asia pacific",
                "asia-pacific",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            offsets: vec![7.0, 8.0, 9.0],
        }
    }

    /// `name` with a reasonable default alias set; offsets default to none (only explicit
    /// mentions count).
    pub fn named(name: &str, offsets: Vec<f64>) -> Self {
        Self {
            name: name.to_string(),
            aliases: vec![name.to_ascii_lowercase()],
            offsets,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Extracted {
    pub records: Vec<NewRecord>,
    /// The next page (Himalayas cursor), when there is one and the cap allows.
    pub next: Vec<Url>,
    pub fingerprint: Vec<u8>,
}

#[derive(Serialize)]
struct Job<'a> {
    platform: &'static str,
    board: &'static str,
    job_id: String,
    url: String,
    title: &'a str,
    company: &'a str,
    /// What the board says about where applicants may live, verbatim.
    location: String,
    /// UTC offsets the board allows, when it says.
    timezones: Vec<f64>,
    employment_type: Option<&'a str>,
    seniority: Option<&'a str>,
    categories: Vec<&'a str>,
    salary: Option<String>,
    /// Unix seconds.
    posted_at: Option<i64>,
    /// Whether the configured country is allowed, by the rule in the module doc.
    eligible_from: Eligibility,
    /// First ~600 chars of the description, tags stripped.
    excerpt: String,
    attribution: String,
}

#[derive(Serialize, Debug, PartialEq, Eq)]
struct Eligibility {
    country: String,
    eligible: bool,
    /// Which rule fired: named, region, worldwide, unrestricted, timezone, or excluded.
    reason: &'static str,
}

/// Which board a URL belongs to, by host and path.
pub fn classify(url: &Url) -> Option<Board> {
    let host = url.host_str()?.trim_start_matches("www.");
    match (host, url.path()) {
        ("himalayas.app", "/jobs/api") => Some(Board::Himalayas),
        ("remotive.com", "/api/remote-jobs") => Some(Board::Remotive),
        ("remoteok.com", "/api") | ("remoteok.io", "/api") => Some(Board::RemoteOk),
        _ => None,
    }
}

/// Extracts jobs from a 2xx body of `url`. `max_jobs` caps how far Himalayas is paged.
pub fn extract(
    url: &Url,
    body: &[u8],
    country: &Country,
    max_jobs: u32,
) -> Result<Extracted, String> {
    let board = classify(url).ok_or_else(|| "not a known job board feed".to_string())?;
    let data: Value =
        serde_json::from_str(std::str::from_utf8(body).map_err(|_| "feed is not UTF-8")?)
            .map_err(|_| "feed is not JSON (challenge page or outage)".to_string())?;
    let (items, next): (Vec<Value>, Vec<Url>) = match board {
        Board::Himalayas => {
            let jobs = data["jobs"].as_array().cloned().unwrap_or_default();
            if jobs.is_empty() && data["totalCount"].is_null() {
                return Err("Himalayas feed without a jobs array".into());
            }
            let offset = url
                .query_pairs()
                .find(|(k, _)| k == "cursor_offset")
                .and_then(|(_, v)| v.parse::<u32>().ok())
                .unwrap_or(0);
            let fetched_so_far = offset + jobs.len() as u32;
            let next = match data["nextCursor"].as_str() {
                Some(cursor) if !jobs.is_empty() && fetched_so_far < max_jobs => {
                    let mut u = url.clone();
                    u.set_query(Some(&format!(
                        "limit={HIMALAYAS_PAGE}&cursor={cursor}&cursor_offset={fetched_so_far}"
                    )));
                    vec![u]
                }
                _ => Vec::new(),
            };
            (jobs, next)
        }
        Board::Remotive => (
            data["jobs"]
                .as_array()
                .cloned()
                .ok_or_else(|| "Remotive feed without a jobs array".to_string())?,
            Vec::new(),
        ),
        Board::RemoteOk => {
            let arr = data
                .as_array()
                .ok_or_else(|| "Remote OK feed is not an array".to_string())?;
            // Element 0 is the legal notice.
            (arr.iter().skip(1).cloned().collect(), Vec::new())
        }
    };
    let mut records = Vec::with_capacity(items.len());
    let mut ids = Vec::with_capacity(items.len());
    for item in &items {
        if let Some(job) = job_from(board, item, country) {
            ids.push(job.job_id.clone());
            records.push(NewRecord {
                kind: "job".into(),
                key: job.job_id.clone(),
                data: serde_json::to_value(&job).expect("plain struct"),
            });
        }
    }
    // The feed's identity is the set of listings on it; order and counters do not matter.
    ids.sort_unstable();
    Ok(Extracted {
        fingerprint: format!("jobs\n{}\n{}", board.name(), ids.join("\n")).into_bytes(),
        records,
        next,
    })
}

fn job_from<'a>(board: Board, v: &'a Value, country: &Country) -> Option<Job<'a>> {
    let s = |k: &str| v[k].as_str();
    let (
        id,
        url,
        title,
        company,
        location,
        timezones,
        employment,
        seniority,
        categories,
        salary,
        posted,
        description,
    ) = match board {
        Board::Himalayas => {
            let url = s("guid").or_else(|| s("applicationLink"))?;
            let locs: Vec<&str> = v["locationRestrictions"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let tz: Vec<f64> = v["timezoneRestrictions"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_f64).collect())
                .unwrap_or_default();
            let cats: Vec<&str> = v["categories"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let salary = match (
                v["minSalary"].as_f64(),
                v["maxSalary"].as_f64(),
                s("currency"),
                s("salaryPeriod"),
            ) {
                (Some(lo), Some(hi), cur, per) => Some(
                    format!(
                        "{}–{} {} {}",
                        lo as i64,
                        hi as i64,
                        cur.unwrap_or(""),
                        per.unwrap_or("")
                    )
                    .trim()
                    .to_string(),
                ),
                _ => None,
            };
            (
                format!(
                    "himalayas:{}",
                    url.trim_end_matches('/').rsplit('/').next().unwrap_or(url)
                ),
                url.split("?utm_").next().unwrap_or(url).to_string(),
                s("title")?,
                s("companyName").unwrap_or(""),
                locs.join(", "),
                tz,
                s("employmentType"),
                s("seniority"),
                cats,
                salary,
                v["pubDate"].as_i64(),
                s("description").or_else(|| s("excerpt")).unwrap_or(""),
            )
        }
        Board::Remotive => {
            let id = v["id"].as_i64()?;
            let tags: Vec<&str> = v["tags"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            (
                format!("remotive:{id}"),
                s("url")?.to_string(),
                s("title")?,
                s("company_name").unwrap_or(""),
                s("candidate_required_location").unwrap_or("").to_string(),
                Vec::new(),
                s("job_type"),
                None,
                tags,
                s("salary").filter(|x| !x.is_empty()).map(str::to_string),
                s("publication_date").and_then(parse_iso),
                s("description").unwrap_or(""),
            )
        }
        Board::RemoteOk => {
            let id = v["id"]
                .as_str()
                .map(str::to_string)
                .or_else(|| v["id"].as_i64().map(|n| n.to_string()))?;
            let tags: Vec<&str> = v["tags"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let salary = match (v["salary_min"].as_f64(), v["salary_max"].as_f64()) {
                (Some(lo), Some(hi)) if hi > 0.0 => {
                    Some(format!("{}–{} USD/year", lo as i64, hi as i64))
                }
                _ => None,
            };
            (
                format!("remoteok:{id}"),
                s("url")?.to_string(),
                s("position")?,
                s("company").unwrap_or(""),
                s("location").unwrap_or("").to_string(),
                Vec::new(),
                None,
                None,
                tags,
                salary,
                v["epoch"].as_i64(),
                s("description").unwrap_or(""),
            )
        }
    };
    let eligible_from = eligibility(&location, &timezones, country);
    Some(Job {
        platform: "web",
        board: board.name(),
        job_id: id,
        url,
        title,
        company,
        location,
        timezones,
        employment_type: employment,
        seniority,
        categories,
        salary,
        posted_at: posted,
        eligible_from,
        excerpt: excerpt(description),
        attribution: format!(
            "Listed on {}; link back to the listing when sharing.",
            board.name()
        ),
    })
}

/// The rule from the module doc.
fn eligibility(location: &str, timezones: &[f64], country: &Country) -> Eligibility {
    let loc = location.to_ascii_lowercase();
    let words: Vec<&str> = loc
        .split(|c: char| !c.is_alphanumeric() && c != '-')
        .filter(|w| !w.is_empty())
        .collect();
    let has = |needle: &str| {
        if needle.contains(' ') || needle.contains('-') {
            loc.contains(needle)
        } else {
            words.contains(&needle)
        }
    };
    let name = country.name.to_ascii_lowercase();
    let (eligible, reason) = if has(&name) {
        (true, "named")
    } else if ["worldwide", "anywhere", "global", "remote", "international"]
        .iter()
        .any(|k| has(k))
        && !loc.contains("except")
        && !loc.contains("excluding")
    {
        (true, "worldwide")
    } else if country.aliases.iter().any(|a| a != &name && has(a)) {
        (true, "region")
    } else if location.trim().is_empty() {
        if timezones.is_empty() {
            (true, "unrestricted")
        } else if country.offsets.iter().any(|o| timezones.contains(o)) {
            (true, "timezone")
        } else {
            (false, "timezone")
        }
    } else {
        (false, "excluded")
    };
    Eligibility {
        country: country.name.clone(),
        eligible,
        reason,
    }
}

fn excerpt(description: &str) -> String {
    let mut text = String::with_capacity(description.len().min(800));
    let mut in_tag = false;
    for ch in description.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                text.push(' ');
            }
            _ if !in_tag => text.push(ch),
            _ => {}
        }
    }
    let text = crate::html::decode_entities(&text);
    let collapsed: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = collapsed.chars().take(600).collect();
    if collapsed.chars().count() > 600 {
        out.push('…');
    }
    out
}

/// `2026-11-06T08:51:01` (Remotive, no zone; treated as UTC) to Unix seconds.
fn parse_iso(s: &str) -> Option<i64> {
    let date = s.get(..10)?;
    let time = s.get(11..19).unwrap_or("00:00:00");
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let mut t = time.split(':').map(|p| p.parse::<i64>().ok());
    let (h, mi, sec) = (t.next()??, t.next()??, t.next()??);
    Some(days_from_civil(y, m, day) * 86_400 + h * 3600 + mi * 60 + sec)
}

/// Howard Hinnant's days_from_civil.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idn() -> Country {
        Country::indonesia()
    }

    #[test]
    fn eligibility_rule_covers_each_case() {
        let e = |loc: &str, tz: &[f64]| eligibility(loc, tz, &idn());
        assert_eq!(
            (e("Indonesia", &[]).eligible, e("Indonesia", &[]).reason),
            (true, "named")
        );
        assert_eq!(e("Worldwide", &[]).reason, "worldwide");
        assert_eq!(e("Anywhere in the World", &[]).reason, "worldwide");
        assert_eq!(e("APAC", &[]).reason, "region");
        assert_eq!(e("Southeast Asia", &[]).reason, "region");
        assert_eq!(
            (e("", &[]).eligible, e("", &[]).reason),
            (true, "unrestricted")
        );
        assert_eq!(
            (e("", &[-5.0, 7.0]).eligible, e("", &[-5.0, 7.0]).reason),
            (true, "timezone")
        );
        assert_eq!(
            (e("", &[-8.0, -5.0]).eligible, e("", &[-8.0, -5.0]).reason),
            (false, "timezone")
        );
        assert_eq!(
            (
                e("United States", &[]).eligible,
                e("United States", &[]).reason
            ),
            (false, "excluded")
        );
        assert!(!e("USA, Canada, USA timezones", &[]).eligible);
        assert!(
            e("Worldwide except Indonesia", &[]).eligible,
            "named wins over the except clause: the board named the country"
        );
        assert!(
            !e("Worldwide except Russia", &[]).eligible,
            "an except clause disables the worldwide rule"
        );
        assert!(
            !e("Malaysia", &[]).eligible,
            "a neighbour is not the country"
        );
        // "sea" alias must not match inside other words.
        assert!(!e("Seattle", &[]).eligible);
    }

    #[test]
    fn himalayas_feed_pages_by_cursor_up_to_the_cap() {
        let body = serde_json::json!({
            "jobs": [
                {"title":"Product Designer","companyName":"Codekeeper","locationRestrictions":["Indonesia"],"timezoneRestrictions":[],"employmentType":"Full Time","seniority":"Mid","categories":["Design"],"minSalary":40000,"maxSalary":60000,"currency":"USD","salaryPeriod":"year","pubDate":1794114200,"guid":"https://himalayas.app/companies/codekeeper/jobs/product-designer","description":"<p>Design <b>things</b>.</p>"},
                {"title":"Sales Ops","companyName":"RetailNext","locationRestrictions":["South Africa"],"timezoneRestrictions":[2],"pubDate":1794114200,"guid":"https://himalayas.app/companies/retailnext/jobs/sales-ops?utm_source=x"}
            ],
            "totalCount": 117614, "nextCursor": "abc"
        }).to_string();
        let u = Url::parse("https://himalayas.app/jobs/api?limit=20").unwrap();
        let out = extract(&u, body.as_bytes(), &idn(), 100).unwrap();
        assert_eq!(out.records.len(), 2);
        let r = &out.records[0];
        assert_eq!(
            (r.kind.as_str(), r.key.as_str()),
            ("job", "himalayas:product-designer")
        );
        assert_eq!(r.data["eligible_from"]["eligible"], true);
        assert_eq!(r.data["salary"], "40000–60000 USD year");
        assert_eq!(r.data["excerpt"], "Design things .");
        assert_eq!(
            out.records[1].data["url"],
            "https://himalayas.app/companies/retailnext/jobs/sales-ops"
        );
        assert_eq!(out.records[1].data["eligible_from"]["eligible"], false);
        assert_eq!(
            out.next[0].as_str(),
            "https://himalayas.app/jobs/api?limit=20&cursor=abc&cursor_offset=2"
        );
        // At the cap, no next page.
        assert!(
            extract(&u, body.as_bytes(), &idn(), 2)
                .unwrap()
                .next
                .is_empty()
        );
        // Order-independent fingerprint.
        let swapped = body.replace(
            "\"title\":\"Product Designer\"",
            "\"title\":\"Product  Designer\"",
        );
        assert_eq!(
            extract(&u, swapped.as_bytes(), &idn(), 100)
                .unwrap()
                .fingerprint,
            out.fingerprint,
            "a title edit is not a new listing; ids decide"
        );
    }

    #[test]
    fn remotive_and_remoteok_feeds() {
        let remotive = serde_json::json!({"0-legal-notice":"...","jobs":[{"id":123,"url":"https://remotive.com/remote-jobs/x","title":"Rust Engineer","company_name":"Acme","candidate_required_location":"Worldwide","job_type":"full_time","publication_date":"2026-11-06T08:51:01","tags":["rust"],"salary":"","description":"Build <em>crawlers</em>"}]}).to_string();
        let u = Url::parse("https://remotive.com/api/remote-jobs").unwrap();
        let out = extract(&u, remotive.as_bytes(), &idn(), 100).unwrap();
        assert_eq!(out.records[0].key, "remotive:123");
        assert_eq!(out.records[0].data["eligible_from"]["reason"], "worldwide");
        assert_eq!(out.records[0].data["posted_at"], 1793955061);
        assert!(out.next.is_empty());

        let remoteok = serde_json::json!([{"legal":"..."},{"id":"456","url":"https://remoteOK.com/remote-jobs/456","position":"AI Developer","company":"Salvo","location":"","tags":["dev"],"epoch":1794096002,"salary_min":50000,"salary_max":90000,"description":"x"}]).to_string();
        let u = Url::parse("https://remoteok.com/api").unwrap();
        let out = extract(&u, remoteok.as_bytes(), &idn(), 100).unwrap();
        assert_eq!(out.records[0].key, "remoteok:456");
        assert_eq!(
            out.records[0].data["eligible_from"]["reason"],
            "unrestricted"
        );
        assert_eq!(out.records[0].data["salary"], "50000–90000 USD/year");
    }

    #[test]
    fn a_non_json_body_is_refused() {
        let u = Url::parse("https://remotive.com/api/remote-jobs").unwrap();
        assert!(extract(&u, b"<html>verify</html>", &idn(), 9).is_err());
        assert!(classify(&Url::parse("https://example.org/api").unwrap()).is_none());
    }
}

//! Employers' own job boards, through the public APIs their applicant-tracking systems
//! publish. A listing on an aggregator is a copy; these are the originals.
//!
//! Supported, each a documented public endpoint with no key and no robots restriction:
//! - Greenhouse: `https://boards-api.greenhouse.io/v1/boards/<slug>/jobs?content=true`
//! - Lever:      `https://api.lever.co/v0/postings/<slug>?mode=json`
//! - Ashby:      `https://api.ashbyhq.com/posting-api/job-board/<slug>?includeCompensation=true`
//!
//! The source URL is one of those. Records are `job`s with the same shape and eligibility
//! rule as the aggregator feeds (`jobs.rs`), `board` naming the vendor and slug, and `url`
//! the employer's own posting page. An employer's careers page usually links to one of
//! these hosts; `discover` finds the slug in a page's links.

use serde_json::Value;
use url::Url;

use crate::jobs::{Country, Extracted, JobRecord};
use crate::store::NewRecord;

/// Version of the ATS job fingerprint. History: 1 (8 Nov 2026).
pub const FINGERPRINT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Vendor {
    Greenhouse,
    Lever,
    Ashby,
}

impl Vendor {
    pub fn name(&self) -> &'static str {
        match self {
            Vendor::Greenhouse => "Greenhouse",
            Vendor::Lever => "Lever",
            Vendor::Ashby => "Ashby",
        }
    }

    /// The API URL for an employer's board on this vendor.
    pub fn api_url(&self, slug: &str) -> Url {
        let s = match self {
            Vendor::Greenhouse => {
                format!("https://boards-api.greenhouse.io/v1/boards/{slug}/jobs?content=true")
            }
            Vendor::Lever => format!("https://api.lever.co/v0/postings/{slug}?mode=json"),
            Vendor::Ashby => format!(
                "https://api.ashbyhq.com/posting-api/job-board/{slug}?includeCompensation=true"
            ),
        };
        Url::parse(&s).expect("well-formed")
    }
}

/// Vendor and employer slug of an API URL, if it is one.
pub fn classify(url: &Url) -> Option<(Vendor, String)> {
    let host = url.host_str()?;
    let segs: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    match (host, segs.as_slice()) {
        ("boards-api.greenhouse.io", ["v1", "boards", slug, "jobs"]) => {
            Some((Vendor::Greenhouse, slug.to_string()))
        }
        ("api.lever.co", ["v0", "postings", slug]) => Some((Vendor::Lever, slug.to_string())),
        ("api.ashbyhq.com", ["posting-api", "job-board", slug]) => {
            Some((Vendor::Ashby, slug.to_string()))
        }
        _ => None,
    }
}

/// Finds an employer's ATS board in the links of its careers page: `job-boards.greenhouse.io/<slug>`,
/// `boards.greenhouse.io/<slug>`, `jobs.lever.co/<slug>`, `jobs.ashbyhq.com/<slug>`, and the
/// embed forms. Returns the vendor and slug; `Vendor::api_url` gives the URL to crawl.
///
/// Greenhouse short links (`grnh.se/<code>`) only resolve by following them; a page that
/// uses those is handled when the redirect target is crawled (see `company.rs`).
pub fn discover(links: &[Url]) -> Option<(Vendor, String)> {
    for l in links {
        let Some(host) = l.host_str() else { continue };
        let segs: Vec<&str> = l
            .path_segments()
            .map(|s| s.filter(|x| !x.is_empty()).collect())
            .unwrap_or_default();
        // Greenhouse embeds name the board in the query: ...?for=<slug>.
        let embed = if host.ends_with("greenhouse.io") {
            l.query_pairs()
                .find(|(k, _)| k == "for")
                .map(|(_, v)| (Vendor::Greenhouse, v.to_string()))
        } else {
            None
        };
        let found = embed.or_else(|| match host {
            "job-boards.greenhouse.io" | "boards.greenhouse.io" | "job-boards.eu.greenhouse.io" => {
                segs.first().map(|s| (Vendor::Greenhouse, s.to_string()))
            }
            "boards-api.greenhouse.io" => classify(l),
            "jobs.lever.co" | "jobs.eu.lever.co" => {
                segs.first().map(|s| (Vendor::Lever, s.to_string()))
            }
            "jobs.ashbyhq.com" => segs.first().map(|s| (Vendor::Ashby, s.to_string())),
            _ => None,
        });
        if let Some((v, slug)) = found
            && !slug.is_empty()
            && slug != "embed"
        {
            return Some((v, slug));
        }
    }
    None
}

/// Extracts jobs from a 2xx body of an ATS API URL.
pub fn extract(url: &Url, body: &[u8], country: &Country) -> Result<Extracted, String> {
    let (vendor, slug) = classify(url).ok_or_else(|| "not a known ATS API URL".to_string())?;
    let text = std::str::from_utf8(body).map_err(|_| "body is not UTF-8")?;
    let data: Value = serde_json::from_str(text)
        .map_err(|_| "ATS API did not return JSON (board missing or an outage)".to_string())?;
    let board = format!("{} ({slug})", vendor.name());
    let items: Vec<Value> = match vendor {
        Vendor::Greenhouse => {
            if data["status"].as_i64() == Some(404) {
                return Err(format!("Greenhouse has no board '{slug}'"));
            }
            data["jobs"].as_array().cloned().unwrap_or_default()
        }
        Vendor::Lever => data
            .as_array()
            .cloned()
            .ok_or_else(|| format!("Lever has no board '{slug}'"))?,
        Vendor::Ashby => data["jobs"].as_array().cloned().unwrap_or_default(),
    };
    let mut records = Vec::with_capacity(items.len());
    let mut ids = Vec::with_capacity(items.len());
    for item in &items {
        let Some(job) = job_from(&vendor, &slug, &board, item, country) else {
            continue;
        };
        ids.push(job.job_id.clone());
        records.push(NewRecord {
            kind: "job".into(),
            key: job.job_id.clone(),
            data: serde_json::to_value(&job).expect("plain struct"),
        });
    }
    ids.sort_unstable();
    Ok(Extracted {
        fingerprint: format!("ats\n{board}\n{}", ids.join("\n")).into_bytes(),
        records,
        next: Vec::new(),
    })
}

fn job_from(
    vendor: &Vendor,
    slug: &str,
    board: &str,
    v: &Value,
    country: &Country,
) -> Option<JobRecord> {
    let s = |k: &str| v[k].as_str();
    let (
        id,
        url,
        title,
        location,
        remote_flag,
        employment,
        department,
        salary,
        posted,
        description,
    ) = match vendor {
        Vendor::Greenhouse => {
            let id = v["id"].as_i64()?;
            let offices: Vec<&str> = v["offices"]
                .as_array()
                .map(|a| a.iter().filter_map(|o| o["name"].as_str()).collect())
                .unwrap_or_default();
            let loc = v["location"]["name"].as_str().unwrap_or("");
            let location = if offices.is_empty() {
                loc.to_string()
            } else {
                format!("{loc}; {}", offices.join(", "))
            };
            let departments: Vec<&str> = v["departments"]
                .as_array()
                .map(|a| a.iter().filter_map(|d| d["name"].as_str()).collect())
                .unwrap_or_default();
            (
                format!("greenhouse:{slug}:{id}"),
                s("absolute_url")?.to_string(),
                s("title")?,
                location,
                None,
                None,
                departments.first().copied(),
                None,
                s("first_published")
                    .or_else(|| s("updated_at"))
                    .and_then(crate::jobs::parse_iso_lenient),
                s("content")
                    .map(crate::html::decode_entities)
                    .unwrap_or_default(),
            )
        }
        Vendor::Lever => {
            let id = s("id")?;
            let cats = &v["categories"];
            let all: Vec<&str> = cats["allLocations"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let location = if all.is_empty() {
                cats["location"].as_str().unwrap_or("").to_string()
            } else {
                all.join(", ")
            };
            let remote = s("workplaceType").map(|w| w.eq_ignore_ascii_case("remote"));
            (
                format!("lever:{slug}:{id}"),
                s("hostedUrl")?.to_string(),
                s("text")?,
                location,
                remote,
                cats["commitment"].as_str(),
                cats["department"]
                    .as_str()
                    .or_else(|| cats["team"].as_str()),
                None,
                v["createdAt"].as_f64().map(|ms| (ms / 1000.0) as i64),
                s("descriptionPlain").unwrap_or("").to_string(),
            )
        }
        Vendor::Ashby => {
            if v["isListed"] == Value::Bool(false) {
                return None;
            }
            let id = s("id")?;
            let mut locs: Vec<&str> = s("location").into_iter().collect();
            if let Some(sec) = v["secondaryLocations"].as_array() {
                locs.extend(sec.iter().filter_map(|l| l["location"].as_str()));
            }
            let remote = v["isRemote"].as_bool();
            let salary = v["compensation"]["compensationTierSummary"]
                .as_str()
                .map(str::to_string);
            (
                format!("ashby:{slug}:{id}"),
                s("jobUrl")?.to_string(),
                s("title")?,
                locs.join(", "),
                remote,
                s("employmentType"),
                s("department"),
                salary,
                s("publishedAt").and_then(crate::jobs::parse_iso_lenient),
                s("descriptionPlain")
                    .or_else(|| s("descriptionHtml"))
                    .unwrap_or("")
                    .to_string(),
            )
        }
    };
    // The vendor's remote flag is authoritative when present: a posting flagged not remote
    // is not eligible anywhere else, whatever the text says. Otherwise the posting body's
    // own hiring-countries sentence decides, then the location field.
    let (mut eligible_from, hiring_sentence) =
        crate::jobs::eligibility_with_body(&location, &[], &plain_text(&description), country);
    if remote_flag == Some(false) {
        eligible_from = crate::jobs::Eligibility::excluded(country, "on-site");
    }
    Some(JobRecord {
        platform: "web",
        board: board.to_string(),
        job_id: id,
        url,
        title: title.to_string(),
        company: slug.to_string(),
        location,
        timezones: Vec::new(),
        employment_type: employment.map(str::to_string),
        seniority: None,
        categories: department.into_iter().map(str::to_string).collect(),
        salary,
        posted_at: posted,
        eligible_from,
        excerpt: crate::jobs::excerpt_of(&description),
        attribution: format!("From the employer's own {} board.", vendor.name()),
        hiring_sentence,
    })
}

/// Tags stripped, entities decoded, whitespace collapsed.
fn plain_text(html_or_text: &str) -> String {
    let mut out = String::with_capacity(html_or_text.len());
    let mut in_tag = false;
    for ch in html_or_text.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    crate::html::decode_entities(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idn() -> Country {
        Country::indonesia()
    }

    #[test]
    fn classifies_and_discovers_boards() {
        assert_eq!(
            classify(
                &Url::parse(
                    "https://boards-api.greenhouse.io/v1/boards/wikimedia/jobs?content=true"
                )
                .unwrap()
            ),
            Some((Vendor::Greenhouse, "wikimedia".into()))
        );
        assert_eq!(
            classify(&Url::parse("https://api.lever.co/v0/postings/toptal?mode=json").unwrap()),
            Some((Vendor::Lever, "toptal".into()))
        );
        assert_eq!(
            classify(&Url::parse("https://api.ashbyhq.com/posting-api/job-board/acme").unwrap()),
            Some((Vendor::Ashby, "acme".into()))
        );
        assert_eq!(
            classify(&Url::parse("https://example.org/jobs").unwrap()),
            None
        );
        let links = [
            Url::parse("https://example.org/about").unwrap(),
            Url::parse("https://jobs.ashbyhq.com/acme").unwrap(),
        ];
        assert_eq!(discover(&links), Some((Vendor::Ashby, "acme".into())));
        let gh = [Url::parse("https://boards.greenhouse.io/embed/job_board?for=stripe").unwrap()];
        assert_eq!(discover(&gh), Some((Vendor::Greenhouse, "stripe".into())));
        assert_eq!(
            Vendor::Lever.api_url("toptal").as_str(),
            "https://api.lever.co/v0/postings/toptal?mode=json"
        );
    }

    #[test]
    fn greenhouse_lever_and_ashby_bodies_become_jobs() {
        let gh = serde_json::json!({"jobs":[{"id":8248898,"title":"Communications Lead","absolute_url":"https://job-boards.greenhouse.io/wikimedia/jobs/8248898","location":{"name":"Remote"},"first_published":"2026-10-07T10:35:54-04:00","offices":[{"name":"Remote"}],"departments":[{"name":"Comms"}],"content":"&lt;p&gt;Hello&lt;/p&gt;"}]}).to_string();
        let out = extract(
            &Url::parse("https://boards-api.greenhouse.io/v1/boards/wikimedia/jobs?content=true")
                .unwrap(),
            gh.as_bytes(),
            &idn(),
        )
        .unwrap();
        let r = &out.records[0];
        assert_eq!(r.key, "greenhouse:wikimedia:8248898");
        assert_eq!(r.data["board"], "Greenhouse (wikimedia)");
        assert_eq!(
            r.data["eligible_from"]["reason"], "worldwide",
            "'Remote' with no country named"
        );
        assert_eq!(r.data["excerpt"], "Hello");
        assert_eq!(r.data["categories"][0], "Comms");

        let lever = serde_json::json!([{"id":"a9","text":"Account Executive","hostedUrl":"https://jobs.lever.co/toptal/a9","workplaceType":"remote","createdAt":1789027672366.0,"categories":{"commitment":"Full-time","department":"Customer","location":"Canada","allLocations":["Canada","Central America"]},"descriptionPlain":"Sell."}]).to_string();
        let out = extract(
            &Url::parse("https://api.lever.co/v0/postings/toptal?mode=json").unwrap(),
            lever.as_bytes(),
            &idn(),
        )
        .unwrap();
        let r = &out.records[0];
        assert_eq!(r.key, "lever:toptal:a9");
        assert_eq!(r.data["location"], "Canada, Central America");
        assert_eq!(r.data["eligible_from"]["eligible"], false);
        assert_eq!(r.data["posted_at"], 1789027672);

        let ashby = serde_json::json!({"jobs":[
            {"id":"x1","title":"Solutions Architect","location":"San Francisco","secondaryLocations":[],"isRemote":true,"workplaceType":"Remote","employmentType":"FullTime","department":"GTM","compensation":{"compensationTierSummary":"$160K – $300K"},"publishedAt":"2026-11-06T00:17:51.024+00:00","jobUrl":"https://jobs.ashbyhq.com/acme/x1","isListed":true},
            {"id":"x2","title":"Recruiter","location":"San Francisco","isRemote":false,"workplaceType":"OnSite","jobUrl":"https://jobs.ashbyhq.com/acme/x2","isListed":true},
            {"id":"x3","title":"Hidden","location":"","jobUrl":"https://jobs.ashbyhq.com/acme/x3","isListed":false}
        ]}).to_string();
        let out = extract(
            &Url::parse("https://api.ashbyhq.com/posting-api/job-board/acme").unwrap(),
            ashby.as_bytes(),
            &idn(),
        )
        .unwrap();
        assert_eq!(out.records.len(), 2, "unlisted postings are skipped");
        assert_eq!(out.records[0].data["salary"], "$160K – $300K");
        assert_eq!(
            out.records[0].data["eligible_from"]["eligible"], false,
            "remote but located in SF: the text names a place that is not the configured country"
        );
        assert_eq!(out.records[1].data["eligible_from"]["reason"], "on-site");
    }

    #[test]
    fn missing_boards_are_errors_not_empty_content() {
        let u = Url::parse("https://boards-api.greenhouse.io/v1/boards/nope/jobs").unwrap();
        assert!(extract(&u, br#"{"status":404,"error":"Job not found"}"#, &idn()).is_err());
        let u = Url::parse("https://api.lever.co/v0/postings/nope").unwrap();
        assert!(extract(&u, br#"{"ok":false}"#, &idn()).is_err());
        assert!(extract(&u, b"<html>", &idn()).is_err());
    }
}

//! `rayap serve`: a read-only HTTP API and a one-page dashboard over rayap databases.
//!
//! A *collection* is one crawl database under a name (`grants`, `tiktok`, ...). The server
//! opens each on demand per request, read-only, so a crawl can write to the same file while
//! the dashboard reads it (SQLite WAL allows one writer and many readers).
//!
//! Routes:
//! - `GET /`                               the dashboard
//! - `GET /api/collections`                names, summaries
//! - `GET /api/{c}/pages?q=`               known pages with their latest state
//! - `GET /api/{c}/events?after=&limit=`   change events, oldest first
//! - `GET /api/{c}/records?q=&kind=&limit=` extracted records, newest first
//!
//! Nothing here writes. Crawling stays in `rayap crawl`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use rayap_core::Store;
use serde::Deserialize;
use serde_json::json;

const INDEX_HTML: &str = include_str!("index.html");

#[derive(Clone)]
pub struct Collections(Arc<BTreeMap<String, PathBuf>>);

impl Collections {
    /// `name=path` pairs. A collection whose file does not exist yet is listed but empty.
    pub fn new(pairs: impl IntoIterator<Item = (String, PathBuf)>) -> Self {
        Self(Arc::new(pairs.into_iter().collect()))
    }

    fn open(&self, name: &str) -> Result<Store, (StatusCode, String)> {
        let path = self
            .0
            .get(name)
            .ok_or_else(|| (StatusCode::NOT_FOUND, format!("no collection '{name}'")))?;
        if !path.exists() {
            return Err((
                StatusCode::NOT_FOUND,
                format!(
                    "collection '{name}' has no database yet ({})",
                    path.display()
                ),
            ));
        }
        Store::open(path).map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))
    }
}

pub fn router(collections: Collections) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/collections", get(list_collections))
        .route("/api/{collection}/pages", get(pages))
        .route("/api/{collection}/events", get(events))
        .route("/api/{collection}/records", get(records))
        .with_state(collections)
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn list_collections(State(c): State<Collections>) -> Response {
    let mut out = Vec::new();
    for (name, path) in c.0.iter() {
        let summary = if path.exists() {
            match Store::open(path).and_then(|s| s.summary()) {
                Ok(s) => serde_json::to_value(s).unwrap_or(json!(null)),
                Err(e) => json!({ "error": e.to_string() }),
            }
        } else {
            json!(null)
        };
        out.push(json!({ "name": name, "db": path.display().to_string(), "summary": summary }));
    }
    json_response(json!(out))
}

#[derive(Deserialize)]
struct PagesQuery {
    q: Option<String>,
}

async fn pages(
    State(c): State<Collections>,
    Path(collection): Path<String>,
    Query(query): Query<PagesQuery>,
) -> Response {
    let store = match c.open(&collection) {
        Ok(s) => s,
        Err((status, message)) => return error(status, message),
    };
    let q = query.q.as_deref().filter(|q| !q.trim().is_empty());
    match store.pages(q) {
        Ok(rows) => json_response(json!(rows)),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
struct EventsQuery {
    after: Option<i64>,
    limit: Option<usize>,
}

async fn events(
    State(c): State<Collections>,
    Path(collection): Path<String>,
    Query(query): Query<EventsQuery>,
) -> Response {
    let store = match c.open(&collection) {
        Ok(s) => s,
        Err((status, message)) => return error(status, message),
    };
    let limit = query.limit.unwrap_or(200).min(5000);
    match store.events_after(query.after.unwrap_or(0), limit) {
        Ok(rows) => json_response(json!(rows)),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

#[derive(Deserialize)]
struct RecordsQuery {
    q: Option<String>,
    kind: Option<String>,
    limit: Option<usize>,
}

async fn records(
    State(c): State<Collections>,
    Path(collection): Path<String>,
    Query(query): Query<RecordsQuery>,
) -> Response {
    let store = match c.open(&collection) {
        Ok(s) => s,
        Err((status, message)) => return error(status, message),
    };
    let q = query.q.as_deref().filter(|q| !q.trim().is_empty());
    let kind = query.kind.as_deref().filter(|k| !k.trim().is_empty());
    let limit = query.limit.unwrap_or(200).min(5000);
    match store.search_records(q, kind, limit) {
        Ok(rows) => json_response(json!(rows)),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
    }
}

fn json_response(value: serde_json::Value) -> Response {
    (
        [(header::CONTENT_TYPE, "application/json")],
        value.to_string(),
    )
        .into_response()
}

fn error(status: StatusCode, message: String) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        json!({ "error": message }).to_string(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use rayap_core::store::{NewPage, NewRecord, Outcome};
    use tower::ServiceExt;

    fn seeded_db(dir: &std::path::Path) -> PathBuf {
        let path = dir.join("grants.db");
        let mut s = Store::open(&path).unwrap();
        let run = s.begin_run(&["nlnet"], None).unwrap().run;
        let page = NewPage {
            url: "https://nlnet.nl/propose/".into(),
            source: "nlnet".into(),
            depth: 0,
            hops: 0,
            continuation: false,
        };
        s.enqueue(run, &page).unwrap();
        let outcome = Outcome {
            status: Some(200),
            title: Some("NLnet; Apply for a grant".into()),
            hash: Some("h1".into()),
            records: vec![NewRecord {
                kind: "post".into(),
                key: "1".into(),
                data: json!({ "text": "frogs in minecraft" }),
            }],
            ..Outcome::default()
        };
        s.complete(
            run,
            "https://nlnet.nl/propose/",
            None,
            outcome,
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        s.finish_run(run).unwrap();
        path
    }

    async fn get_json(app: Router, uri: &str) -> (StatusCode, serde_json::Value) {
        let response = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(json!(null)),
        )
    }

    #[tokio::test]
    async fn lists_collections_pages_events_and_records() {
        let dir = tempfile::tempdir().unwrap();
        let db = seeded_db(dir.path());
        let app = router(Collections::new([
            ("grants".to_string(), db),
            ("tiktok".to_string(), dir.path().join("missing.db")),
        ]));

        let (status, v) = get_json(app.clone(), "/api/collections").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v[0]["name"], "grants");
        assert_eq!(v[0]["summary"]["pages_with_content"], 1);
        assert_eq!(v[1]["name"], "tiktok");
        assert!(v[1]["summary"].is_null(), "no database yet");

        let (status, v) = get_json(app.clone(), "/api/grants/pages?q=nlnet").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v[0]["title"], "NLnet; Apply for a grant");
        assert_eq!(v[0]["last_kind"], "new");
        assert_eq!(v[0]["records"], 1);
        let (_, v) = get_json(app.clone(), "/api/grants/pages?q=zzz").await;
        assert_eq!(v.as_array().unwrap().len(), 0);

        let (_, v) = get_json(app.clone(), "/api/grants/events").await;
        assert_eq!(v[0]["kind"], "new");

        let (_, v) = get_json(app.clone(), "/api/grants/records?q=frogs").await;
        assert_eq!(v[0]["data"]["text"], "frogs in minecraft");
        let (_, v) = get_json(app.clone(), "/api/grants/records?kind=comment").await;
        assert_eq!(v.as_array().unwrap().len(), 0);

        let (status, _) = get_json(app.clone(), "/api/nope/pages").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = get_json(app.clone(), "/api/tiktok/pages").await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let response = app
            .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}

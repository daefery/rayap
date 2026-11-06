//! `rayap-sim`: serve the simulated web. Prints `listening <addr>` when ready.
//! `GET /__sim/report` on any non-`.sim` host returns the referee's report as JSON.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::body::Body as AxumBody;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response as AxumResponse};
use clap::Parser;
use rayap_sim::{Body, Referee, WebSpec, filler, parse_host};

#[derive(Parser)]
#[command(about = "Serve a simulated web for crawler benchmarks")]
struct Cli {
    #[command(flatten)]
    spec: WebSpec,
    /// Address to listen on. Every 127.x.y.z address reaches it on Linux.
    #[arg(long, default_value = "0.0.0.0:0")]
    listen: SocketAddr,
}

struct Sim {
    spec: WebSpec,
    referee: Referee,
    filler: String,
}

async fn handle(State(sim): State<Arc<Sim>>, request: Request) -> AxumResponse {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let path = request
        .uri()
        .path_and_query()
        .map(|p| p.to_string())
        .unwrap_or_else(|| "/".into());
    let Some(h) = parse_host(&host).filter(|h| *h < sim.spec.hosts) else {
        if path == "/__sim/report" {
            return axum::Json(sim.referee.report()).into_response();
        }
        sim.referee.unknown_host();
        return StatusCode::MISDIRECTED_REQUEST.into_response();
    };

    let attempt = sim.referee.start(&sim.spec, h, &path, Instant::now());
    let response = sim.spec.respond(h, &path, attempt, &sim.filler);
    tokio::time::sleep(response.latency).await;

    let (content_type, bytes) = match &response.body {
        Body::Html(s) => ("text/html; charset=utf-8", s.clone().into_bytes()),
        Body::Text(s) => ("text/plain", s.clone().into_bytes()),
        Body::Empty => ("text/plain", Vec::new()),
    };
    // The response is complete from the server's side once it is built; record the end now so
    // the crawler's next request is judged against it. (The crawler finishes reading later,
    // so this can only make the server stricter, never more lenient.)
    sim.referee.end(h, &response, bytes.len(), Instant::now());

    let mut out = AxumResponse::new(AxumBody::from(bytes));
    *out.status_mut() =
        StatusCode::from_u16(response.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    out.headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    for (name, value) in response.headers {
        if let Ok(v) = HeaderValue::from_str(&value) {
            out.headers_mut().insert(name, v);
        }
    }
    out
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let sim = Arc::new(Sim {
        referee: Referee::new(cli.spec.hosts),
        filler: filler(cli.spec.body_kib as usize * 1024 + 1024),
        spec: cli.spec,
    });
    let app = Router::new().fallback(handle).with_state(sim);
    let listener = tokio::net::TcpListener::bind(cli.listen)
        .await
        .expect("bind");
    println!("listening {}", listener.local_addr().expect("addr"));
    axum::serve(listener, app).await.expect("serve");
}

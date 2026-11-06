//! `rayap-sim`: serve the simulated web. Prints `listening <addr>` when ready.
//! `GET /__sim/report` on any non-`.sim` host returns the referee's report as JSON.

use std::net::SocketAddr;
use std::sync::Arc;

use clap::Parser;
use rayap_sim::WebSpec;
use rayap_sim::server::{Sim, router};

#[derive(Parser)]
#[command(about = "Serve a simulated web for crawler benchmarks")]
struct Cli {
    #[command(flatten)]
    spec: WebSpec,
    /// Address to listen on. Every 127.x.y.z address reaches it on Linux.
    #[arg(long, default_value = "0.0.0.0:0")]
    listen: SocketAddr,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let app = router(Arc::new(Sim::new(cli.spec)));
    let listener = tokio::net::TcpListener::bind(cli.listen)
        .await
        .expect("bind");
    println!("listening {}", listener.local_addr().expect("addr"));
    axum::serve(listener, app).await.expect("serve");
}

//! `rayap` command line.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use rayap_core::{Config, Event, EventKind, Options, Record, Store, crawl};

#[derive(Parser)]
#[command(
    name = "rayap",
    version,
    about = "A polite, restartable web crawl engine"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Crawl every source once (resuming an interrupted run) and print change events as JSON lines.
    Crawl {
        /// Crawl configuration (TOML).
        #[arg(short, long, default_value = "rayap.toml")]
        config: PathBuf,
        /// State database; created if missing.
        #[arg(long, default_value = "rayap.db")]
        db: PathBuf,
        /// Print only `new` and `changed` events.
        #[arg(long)]
        changes_only: bool,
        /// Stop after this many page requests (the next run resumes).
        #[arg(long)]
        max_fetches: Option<u64>,
    },
    /// Print stored events with an id greater than AFTER, as JSON lines.
    Events {
        #[arg(long, default_value = "rayap.db")]
        db: PathBuf,
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 1000)]
        limit: usize,
    },
    /// Print extracted records (posts, comments) with an id greater than AFTER, as JSON lines.
    Records {
        #[arg(long, default_value = "rayap.db")]
        db: PathBuf,
        #[arg(long, default_value_t = 0)]
        after: i64,
        #[arg(long, default_value_t = 1000)]
        limit: usize,
        /// Only records of this kind (`post`, `comment`).
        #[arg(long)]
        kind: Option<String>,
    },
    /// Check a configuration file and print the User-Agent it will send.
    Check {
        #[arg(short, long, default_value = "rayap.toml")]
        config: PathBuf,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(code) => code,
        Err(message) => {
            eprintln!("rayap: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode, String> {
    match cli.command {
        Command::Check { config } => {
            let config = load(&config)?;
            println!("ok: {} source(s)", config.sources.len());
            println!("user-agent: {}", config.user_agent());
            Ok(ExitCode::SUCCESS)
        }
        Command::Events { db, after, limit } => {
            let store = Store::open(&db).map_err(|e| e.to_string())?;
            let mut out = io::stdout().lock();
            for event in store
                .events_after(after, limit)
                .map_err(|e| e.to_string())?
            {
                write_event(&mut out, &event)?;
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Records {
            db,
            after,
            limit,
            kind,
        } => {
            let store = Store::open(&db).map_err(|e| e.to_string())?;
            let mut out = io::stdout().lock();
            for record in store
                .records_after(after, kind.as_deref(), limit)
                .map_err(|e| e.to_string())?
            {
                write_record(&mut out, &record)?;
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Crawl {
            config,
            db,
            changes_only,
            max_fetches,
        } => {
            let config = load(&config)?;
            let mut store = Store::open(&db).map_err(|e| e.to_string())?;
            let mut out = io::stdout().lock();
            let mut write_error = None;
            let stats = crawl(
                &config,
                &mut store,
                Options {
                    max_fetches,
                    ..Options::default()
                },
                |event| {
                    let wanted =
                        !changes_only || matches!(event.kind, EventKind::New | EventKind::Changed);
                    if wanted && write_error.is_none() {
                        write_error = write_event(&mut out, event).err();
                    }
                },
            )
            .await
            .map_err(|e| e.to_string())?;
            if let Some(e) = write_error {
                return Err(e);
            }
            eprintln!(
                "rayap: run {}{}: {} requests, {} events, {} changed, {} bytes{}",
                stats.run,
                if stats.resumed { " (resumed)" } else { "" },
                stats.requests,
                stats.events,
                stats.changed,
                stats.bytes,
                if stats.finished {
                    ""
                } else {
                    ", unfinished: run again to resume"
                }
            );
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn load(path: &PathBuf) -> Result<Config, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Config::from_toml(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn write_event(out: &mut impl Write, event: &Event) -> Result<(), String> {
    let line = serde_json::to_string(event).map_err(|e| e.to_string())?;
    writeln!(out, "{line}").map_err(|e| e.to_string())
}

fn write_record(out: &mut impl Write, record: &Record) -> Result<(), String> {
    let line = serde_json::to_string(record).map_err(|e| e.to_string())?;
    writeln!(out, "{line}").map_err(|e| e.to_string())
}

//! `rayap` command line.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use rayap_core::revisit::Prior;
use rayap_core::{Config, Event, EventKind, Options, Plan, Record, Store, crawl};

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
        /// Revisit only this many known pages, the ones most due by their learned change
        /// rate, plus every page never fetched. Without it every known page is visited.
        #[arg(long)]
        budget: Option<usize>,
        /// With --budget: visit every page at least this often, however rarely it changes.
        #[arg(long, default_value_t = 30.0)]
        max_interval_days: f64,
        /// Coordinators to split hosts across. Useful past a few thousand hosts; 1 otherwise.
        #[arg(long, default_value_t = 1)]
        shards: usize,
    },
    /// Print each known page's learned change rate (per day), as JSON lines.
    Rates {
        #[arg(short, long, default_value = "rayap.toml")]
        config: PathBuf,
        #[arg(long, default_value = "rayap.db")]
        db: PathBuf,
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
    /// Signing keys for Web Bot Auth (RFC 9421, Ed25519).
    Key {
        #[command(subcommand)]
        action: KeyAction,
    },
    /// Serve a read-only dashboard and JSON API over one or more crawl databases.
    Serve {
        /// A collection as `name=path/to.db`; repeat for several (e.g. grants=grants.db).
        #[arg(long = "db", required = true, value_parser = parse_collection)]
        collections: Vec<(String, PathBuf)>,
        #[arg(long, default_value = "127.0.0.1:8732")]
        listen: String,
    },
}

#[derive(Subcommand)]
enum KeyAction {
    /// Generate a new Ed25519 seed and write it (base64url) to FILE. Refuses to overwrite.
    New { file: PathBuf },
    /// Print the JWK Set to serve at AGENT/.well-known/http-message-signatures-directory.
    Directory {
        #[arg(short, long, default_value = "rayap.toml")]
        config: PathBuf,
    },
}

fn parse_collection(s: &str) -> Result<(String, PathBuf), String> {
    let (name, path) = s
        .split_once('=')
        .ok_or_else(|| format!("'{s}': expected name=path"))?;
    let name = name.trim();
    let ok = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !ok {
        return Err(format!(
            "'{name}': collection names use letters, digits, '-' and '_'"
        ));
    }
    Ok((name.to_string(), PathBuf::from(path.trim())))
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
        Command::Key {
            action: KeyAction::New { file },
        } => {
            if file.exists() {
                return Err(format!("{}: exists, not overwriting", file.display()));
            }
            use base64::Engine;
            let seed = rayap_core::signing::Signer::generate_seed();
            let text = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(seed);
            std::fs::write(&file, format!("{text}\n"))
                .map_err(|e| format!("{}: {e}", file.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600));
            }
            eprintln!(
                "rayap: wrote a new Ed25519 seed to {}. Keep it private; add to rayap.toml:\n[signing]\nkey_file = \"{}\"\nagent = \"https://your-domain.example\"",
                file.display(),
                file.display()
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Key {
            action: KeyAction::Directory { config },
        } => {
            let config = load(&config)?;
            let Some(signing) = &config.signing else {
                return Err("no [signing] section in the config".into());
            };
            let agent = url::Url::parse(&signing.agent).map_err(|e| e.to_string())?;
            let signer = rayap_core::signing::Signer::from_key_file(
                std::path::Path::new(&signing.key_file),
                agent.clone(),
            )
            .map_err(|e| e.to_string())?;
            eprintln!(
                "rayap: serve this as {}/.well-known/http-message-signatures-directory with\nContent-Type: application/http-message-signatures-directory+json (keyid {})",
                agent.origin().ascii_serialization(),
                signer.keyid()
            );
            println!(
                "{}",
                serde_json::to_string_pretty(&signer.directory()).map_err(|e| e.to_string())?
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Serve {
            collections,
            listen,
        } => {
            let names: Vec<String> = collections
                .iter()
                .map(|(n, p)| format!("{n} ({})", p.display()))
                .collect();
            let app = rayap_serve::router(rayap_serve::Collections::new(collections));
            let listener = tokio::net::TcpListener::bind(&listen)
                .await
                .map_err(|e| format!("{listen}: {e}"))?;
            eprintln!(
                "rayap: serving {} at http://{listen}/ (read-only; ctrl-c to stop)",
                names.join(", ")
            );
            axum::serve(listener, app)
                .await
                .map_err(|e| e.to_string())?;
            Ok(ExitCode::SUCCESS)
        }
        Command::Rates { config, db } => {
            let config = load(&config)?;
            let store = Store::open(&db).map_err(|e| e.to_string())?;
            let mut out = io::stdout().lock();
            for source in &config.sources {
                for (url, rate) in store
                    .rates(&source.name, DEFAULT_PRIOR)
                    .map_err(|e| e.to_string())?
                {
                    let line = serde_json::json!({
                        "source": source.name,
                        "url": url,
                        "changes_per_day": rate,
                    });
                    writeln!(out, "{line}").map_err(|e| e.to_string())?;
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Crawl {
            config,
            db,
            changes_only,
            max_fetches,
            budget,
            max_interval_days,
            shards,
        } => {
            let config = load(&config)?;
            let mut store = Store::open(&db).map_err(|e| e.to_string())?;
            let mut out = io::stdout().lock();
            let mut write_error = None;
            let plan = budget.map(|budget| Plan {
                budget,
                max_interval_days,
                prior: DEFAULT_PRIOR,
            });
            let stats = crawl(
                &config,
                &mut store,
                Options {
                    max_fetches,
                    plan,
                    shards,
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
                "rayap: run {}{}: {} requests, {} events, {} changed, {} bytes{}{}",
                stats.run,
                if stats.resumed { " (resumed)" } else { "" },
                stats.requests,
                stats.events,
                stats.changed,
                stats.bytes,
                if stats.deferred > 0 {
                    format!(", {} pages deferred by the budget", stats.deferred)
                } else {
                    String::new()
                },
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

/// Before any revisit shows a change, a page is assumed to change about once in five days,
/// held weakly (half an observation) so real visits take over quickly.
const DEFAULT_PRIOR: Prior = Prior {
    rate: 0.2,
    weight: 0.5,
};

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

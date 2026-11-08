//! rayap: a polite, restartable web crawl engine.
//!
//! The engine keeps one queue per host and an earliest-eligible-host heap, so a slow or
//! rate-limited host never blocks the others. robots.txt follows RFC 9309, every request
//! carries a contact address, and all state lives in SQLite so an interrupted crawl resumes
//! where it stopped. Each fetched page produces one change event (`new`, `changed`,
//! `unchanged`, `redirect`, `gone`, `blocked`, `error`). Sources with a structured kind
//! (`tiktok`) also yield records (`post`, `comment`) committed with the event.

pub mod company;
pub mod config;
pub mod crawler;
mod fetch;
pub mod frontier;
pub mod html;
pub mod revisit;
pub mod robots;
pub mod store;
pub mod tiktok;
pub mod urlnorm;
pub mod writer;

pub use config::{Config, SourceKind};
pub use crawler::{Options, Stats, crawl};
pub use store::{Event, EventKind, PageView, Plan, Record, Store, Summary};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("config: {0}")]
    Config(String),
    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("http client: {0}")]
    Http(#[from] reqwest::Error),
    #[error("fetch task failed: {0}")]
    Task(String),
}

pub type Result<T> = std::result::Result<T, Error>;

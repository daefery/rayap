//! Browser route: fetch a page by letting a headless browser run its JavaScript, then read
//! the resulting DOM. For pages that are an empty application shell over plain HTTP.
//!
//! What this is: one headless Chrome process per request (`--headless=new --dump-dom`),
//! with a bounded virtual-time budget so scripts get to render and the process always ends.
//! The request still goes through the host's queue, delay and robots.txt check like any
//! other, and the browser is told to identify itself with rayap's user agent. The DOM is
//! then digested by the same extractor and validator as an HTTP body.
//!
//! What this is not: it does not solve captchas, rotate identities, spoof fingerprints or
//! reuse logged-in sessions. A challenge page rendered by the browser is still a challenge
//! page and is handled as such.
//!
//! The browser's own subresource requests (scripts, images, XHR) are not scheduled by rayap.
//! They go to whatever hosts the page asks for, like a visitor's browser would; this route
//! is therefore for sites you have reason to render, not for broad crawls.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use url::Url;

/// Which browser binary to run. Found once at startup; `None` means rendering is unavailable
/// and pages that ask for it are reported as errors, not silently fetched over HTTP.
#[derive(Debug, Clone)]
pub struct Browser {
    binary: PathBuf,
    user_agent: String,
    /// Virtual time the page gets to run scripts before the DOM is read.
    budget: Duration,
    /// Hard wall-clock limit for the whole browser process.
    timeout: Duration,
}

const CANDIDATES: &[&str] = &[
    "chromium",
    "chromium-browser",
    "google-chrome",
    "google-chrome-stable",
    "chrome",
];

impl Browser {
    /// Looks for a Chromium-family binary on `PATH`, or at `RAYAP_BROWSER` if set.
    pub fn find(user_agent: &str, timeout: Duration) -> Option<Self> {
        let explicit = std::env::var_os("RAYAP_BROWSER").map(PathBuf::from);
        let binary = explicit
            .filter(|p| p.is_file())
            .or_else(|| CANDIDATES.iter().find_map(|name| which(name)))?;
        Some(Self {
            binary,
            user_agent: user_agent.to_string(),
            budget: Duration::from_secs(8),
            timeout,
        })
    }

    pub fn binary(&self) -> &std::path::Path {
        &self.binary
    }

    /// Renders `url` and returns the serialised DOM. The browser follows redirects itself,
    /// so the DOM may be another URL's; the caller compares against the configured one.
    pub async fn render(&self, url: &Url) -> std::result::Result<Vec<u8>, String> {
        let budget_ms = self.budget.as_millis();
        let mut command = Command::new(&self.binary);
        command
            .arg("--headless=new")
            .arg("--disable-gpu")
            .arg("--no-sandbox")
            .arg("--no-first-run")
            .arg("--disable-extensions")
            .arg("--disable-background-networking")
            .arg("--mute-audio")
            .arg(format!("--user-agent={}", self.user_agent))
            .arg(format!("--virtual-time-budget={budget_ms}"))
            .arg("--dump-dom")
            .arg(url.as_str())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let output = tokio::time::timeout(self.timeout, command.output())
            .await
            .map_err(|_| format!("browser: timed out after {:?}", self.timeout))?
            .map_err(|e| format!("browser: {e}"))?;
        if !output.status.success() {
            return Err(format!("browser: exit {}", output.status));
        }
        if output.stdout.is_empty() {
            return Err("browser: empty DOM".into());
        }
        Ok(output.stdout)
    }
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_browser_only_if_one_exists() {
        // RAYAP_BROWSER pointing at a non-file is ignored; PATH decides.
        let found = Browser::find("rayap/test", Duration::from_secs(5));
        if let Some(b) = &found {
            assert!(b.binary().is_file());
        }
        // Not asserting presence: CI machines may have no browser. The crawl-level test
        // skips itself in that case.
    }
}

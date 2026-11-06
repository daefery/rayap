//! `rayap-bench`: crawl a simulated web and report measured throughput and politeness.
//!
//! Two modes, reported separately:
//! - `e2e`: the real engine (HTTP, robots.txt, parsing, SQLite) against `rayap-sim`.
//! - `scheduler`: only the frontier, with simulated request durations and no I/O, to show
//!   what the scheduling core alone can sustain.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use rayap_core::frontier::{Frontier, Next};
use rayap_core::{Config, EventKind, Options, Store, crawl};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, BufReader};

#[derive(Parser)]
#[command(about = "Benchmark rayap against a simulated web")]
struct Cli {
    #[command(subcommand)]
    mode: Mode,
}

#[derive(Subcommand)]
enum Mode {
    /// Real engine against `rayap-sim` (started by this command).
    E2e {
        /// Arguments passed to rayap-sim, e.g. "--hosts 10000 --pages 20".
        #[arg(long, default_value = "", allow_hyphen_values = true)]
        sim_args: String,
        #[arg(long, default_value_t = 10_000)]
        hosts: u32,
        #[arg(long, default_value_t = 2_000)]
        delay_ms: u64,
        #[arg(long, default_value_t = 4_096)]
        concurrency: usize,
        /// Stop after this long even if pages remain (the report counts what finished).
        #[arg(long, default_value_t = 300)]
        max_seconds: u64,
        #[arg(long, default_value = "target/bench/e2e.db")]
        db: PathBuf,
        /// Write a CPU flamegraph (SVG) of the crawler process.
        #[arg(long)]
        flamegraph: Option<PathBuf>,
        #[arg(long, default_value = "target/release/rayap-sim")]
        sim_bin: PathBuf,
        /// Negative control: run the crawler with this per-host delay while the referee
        /// enforces --delay-ms, to check that the referee catches a crawler that is too fast.
        #[arg(long)]
        crawler_delay_ms: Option<u64>,
    },
    /// Scheduling core only: frontier with simulated request times, no network or storage.
    Scheduler {
        #[arg(long, default_value_t = 100_000)]
        hosts: u32,
        #[arg(long, default_value_t = 50)]
        pages_per_host: u32,
        #[arg(long, default_value_t = 2_000)]
        delay_ms: u64,
        #[arg(long, default_value_t = 200)]
        request_ms: u64,
        #[arg(long, default_value_t = 100_000)]
        in_flight: usize,
    },
}

/// Resolves `hNNNNN.sim` to that host's own loopback address and the simulator's port.
struct SimResolver {
    port: u16,
}

impl Resolve for SimResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let addr =
            rayap_sim_parse(name.as_str()).map(|h| SocketAddr::from((host_ip(h), self.port)));
        Box::pin(async move {
            match addr {
                Some(a) => Ok(Box::new(std::iter::once(a)) as Addrs),
                None => Err(format!("unknown simulated host {}", name.as_str()).into()),
            }
        })
    }
}

fn rayap_sim_parse(name: &str) -> Option<u32> {
    name.strip_prefix('h')?.strip_suffix(".sim")?.parse().ok()
}

fn host_ip(h: u32) -> std::net::Ipv4Addr {
    let n = h + 1;
    std::net::Ipv4Addr::new(127, 1 + (n >> 16) as u8, (n >> 8) as u8, n as u8)
}

/// CPU seconds (user + system) and peak RSS (MiB) of this process so far.
fn self_usage() -> (f64, f64) {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    let secs = |t: libc::timeval| t.tv_sec as f64 + t.tv_usec as f64 / 1e6;
    (
        secs(u.ru_utime) + secs(u.ru_stime),
        u.ru_maxrss as f64 / 1024.0,
    )
}

/// user + system CPU seconds of another process, from /proc.
fn process_cpu_seconds(pid: u32) -> Option<f64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = after.split_whitespace().collect();
    let ticks: f64 = fields.get(11)?.parse::<f64>().ok()? + fields.get(12)?.parse::<f64>().ok()?;
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    Some(ticks / hz)
}

fn machine() -> serde_json::Value {
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("model name"))
                .map(|l| l.split(':').nth(1).unwrap_or("").trim().to_string())
        })
        .unwrap_or_default();
    let mem_gib = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|kb| kb.parse::<f64>().ok())
        })
        .map_or(0.0, |kb| kb / 1024.0 / 1024.0);
    let kernel = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    json!({
        "cpu": cpu,
        "logical_cpus": std::thread::available_parallelism().map_or(0, |n| n.get()),
        "mem_gib": (mem_gib * 10.0).round() / 10.0,
        "kernel": kernel.trim(),
    })
}

#[tokio::main]
async fn main() {
    match Cli::parse().mode {
        Mode::Scheduler {
            hosts,
            pages_per_host,
            delay_ms,
            request_ms,
            in_flight,
        } => {
            scheduler(hosts, pages_per_host, delay_ms, request_ms, in_flight);
        }
        Mode::E2e {
            sim_args,
            hosts,
            delay_ms,
            concurrency,
            max_seconds,
            db,
            flamegraph,
            sim_bin,
            crawler_delay_ms,
        } => {
            e2e(
                sim_args,
                hosts,
                delay_ms,
                concurrency,
                max_seconds,
                db,
                flamegraph,
                sim_bin,
                crawler_delay_ms,
            )
            .await;
        }
    }
}

/// Runs the frontier against a virtual clock: every request takes `request_ms`, every host
/// needs `delay_ms` between requests, at most `in_flight` requests at once. Measures how many
/// scheduling decisions per second the core makes, and the virtual pages/s the schedule allows.
fn scheduler(hosts: u32, pages_per_host: u32, delay_ms: u64, request_ms: u64, in_flight: usize) {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    use tokio::time::Instant as TInstant;

    let base = TInstant::now();
    let mut now = base;
    let mut f: Frontier<u32> = Frontier::new(Duration::from_millis(delay_ms));
    for p in 0..pages_per_host {
        for h in 0..hosts {
            f.push(&format!("h{h:06}"), p, now);
        }
    }
    let total = u64::from(hosts) * u64::from(pages_per_host);
    let request = Duration::from_millis(request_ms);
    let delay = Duration::from_millis(delay_ms);
    // (finish time, host) for requests in flight.
    let mut running: BinaryHeap<Reverse<(TInstant, String)>> = BinaryHeap::new();
    let mut done = 0u64;
    let wall = Instant::now();
    let (cpu0, _) = self_usage();
    while done < total {
        while running.len() < in_flight {
            match f.next(now) {
                Next::Ready { host, .. } => running.push(Reverse((now + request, host))),
                Next::WaitUntil(_) | Next::Idle => break,
            }
        }
        // Advance the virtual clock to the next event: a request finishing or a host waking.
        let next_finish = running.peek().map(|Reverse((t, _))| *t);
        let next_wake = match f.next(now) {
            Next::WaitUntil(t) => Some(t),
            Next::Ready { host, item } => {
                // Concurrency-limited: put it back and wait for a finish.
                f.push_front(&host, item, now);
                f.release(&host, now, Duration::ZERO);
                None
            }
            Next::Idle => None,
        };
        now = match (next_finish, next_wake) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => break,
        };
        while let Some(Reverse((t, _))) = running.peek() {
            if *t > now {
                break;
            }
            let Some(Reverse((t, host))) = running.pop() else {
                break;
            };
            f.release(&host, t, delay);
            done += 1;
        }
    }
    let elapsed = wall.elapsed().as_secs_f64();
    let (cpu1, rss) = self_usage();
    let virtual_secs = (now - base).as_secs_f64();
    let report = json!({
        "mode": "scheduler",
        "machine": machine(),
        "params": { "hosts": hosts, "pages_per_host": pages_per_host, "delay_ms": delay_ms, "request_ms": request_ms, "in_flight": in_flight },
        "pages": done,
        "wall_seconds": round(elapsed),
        "scheduling_decisions_per_wall_second": (done as f64 / elapsed).round(),
        "virtual_seconds": round(virtual_secs),
        "virtual_pages_per_second": round(done as f64 / virtual_secs),
        "cpu_seconds": round(cpu1 - cpu0),
        "peak_rss_mib": round(rss),
        "note": "No network, parsing or storage. Measures the frontier only.",
    });
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}

fn round(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

#[allow(clippy::too_many_arguments)]
async fn e2e(
    sim_args: String,
    hosts: u32,
    delay_ms: u64,
    concurrency: usize,
    max_seconds: u64,
    db: PathBuf,
    flamegraph: Option<PathBuf>,
    sim_bin: PathBuf,
    crawler_delay_ms: Option<u64>,
) {
    let crawler_delay = crawler_delay_ms.unwrap_or(delay_ms);
    // Start the simulator and wait for its address.
    let mut args: Vec<String> = sim_args.split_whitespace().map(String::from).collect();
    args.extend([
        "--hosts".into(),
        hosts.to_string(),
        "--delay-ms".into(),
        delay_ms.to_string(),
    ]);
    let mut sim = tokio::process::Command::new(&sim_bin)
        .args(&args)
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap_or_else(|e| {
            panic!(
                "start {}: {e} (build it with cargo build --release)",
                sim_bin.display()
            )
        });
    let mut lines = BufReader::new(sim.stdout.take().unwrap()).lines();
    let addr: SocketAddr = loop {
        let line = lines.next_line().await.unwrap().expect("simulator exited");
        if let Some(a) = line.strip_prefix("listening ") {
            break a.parse().unwrap();
        }
    };
    let port = addr.port();
    let sim_pid = sim.id();
    let sim_spec: serde_json::Value = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/__sim/spec"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .map(|b| serde_json::from_slice(&b).unwrap_or_default())
        .unwrap_or_default();
    let started_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());

    let config = Config::from_toml(&format!(
        r#"
        [crawler]
        contact = "mailto:bench@example.invalid"
        per_host_delay_ms = {crawler_delay}
        max_concurrency = {concurrency}
        request_timeout_ms = 30000
        retry_backoff_ms = 2000
        max_wait_ms = 60000
        {sources}
        "#,
        sources = (0..hosts)
            .map(|h| format!(
                "[[source]]\nname = \"h{h}\"\nurl = \"http://h{h:05}.sim/\"\nmax_depth = 2\n"
            ))
            .collect::<String>()
    ))
    .expect("config");

    let _ = std::fs::create_dir_all(db.parent().unwrap_or(std::path::Path::new(".")));
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
    }
    let mut store = Store::open(&db).expect("db");

    let guard = flamegraph.as_ref().map(|_| {
        pprof::ProfilerGuardBuilder::default()
            .frequency(499)
            .blocklist(&["libc", "libgcc", "pthread", "vdso"])
            .build()
            .unwrap()
    });

    // Every second, record cumulative pages, crawler CPU seconds and coordinator busy time,
    // so throughput, CPU and busy fraction can be reported over the same window.
    let pages = Arc::new(AtomicU64::new(0));
    let busy = Arc::new(AtomicU64::new(0));
    // (seconds since start, pages, crawler CPU seconds, coordinator busy microseconds)
    let samples = Arc::new(std::sync::Mutex::new(Vec::<(f64, u64, f64, u64)>::new()));
    let origin = tokio::time::Instant::now();
    let sampler = {
        let pages = Arc::clone(&pages);
        let busy = Arc::clone(&busy);
        let samples = Arc::clone(&samples);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.tick().await;
            loop {
                tick.tick().await;
                let t = origin.elapsed().as_secs_f64();
                let cpu = self_usage().0;
                samples.lock().unwrap().push((
                    t,
                    pages.load(Ordering::Relaxed),
                    cpu,
                    busy.load(Ordering::Relaxed),
                ));
            }
        })
    };

    let (cpu0, _) = self_usage();
    samples.lock().unwrap().push((0.0, 0, cpu0, 0));
    let started = Instant::now();
    let mut kinds = std::collections::BTreeMap::<&'static str, u64>::new();
    let options = Options {
        resolver: Some(Arc::new(SimResolver { port })),
        busy_micros: Some(Arc::clone(&busy)),
        ..Options::default()
    };
    let crawl_fut = crawl(&config, &mut store, options, |e| {
        *kinds.entry(e.kind.as_str()).or_default() += 1;
        if matches!(
            e.kind,
            EventKind::New | EventKind::Changed | EventKind::Unchanged
        ) {
            pages.fetch_add(1, Ordering::Relaxed);
        }
    });
    let stats = match tokio::time::timeout(Duration::from_secs(max_seconds), crawl_fut).await {
        Ok(r) => Some(r.expect("crawl")),
        Err(_) => None,
    };
    let elapsed = started.elapsed().as_secs_f64();
    let (cpu1, rss) = self_usage();
    sampler.abort();
    // Final sample at the exact end, so the whole-run window includes the last partial second.
    samples.lock().unwrap().push((
        origin.elapsed().as_secs_f64(),
        pages.load(Ordering::Relaxed),
        cpu1,
        busy.load(Ordering::Relaxed),
    ));

    if let (Some(guard), Some(path)) = (guard, &flamegraph)
        && let Ok(report) = guard.report().build()
    {
        let file = std::fs::File::create(path).unwrap();
        report.flamegraph(file).unwrap();
    }

    let referee: serde_json::Value = reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/__sim/report"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .map(|b| serde_json::from_slice(&b).unwrap_or_default())
        .unwrap_or_default();
    let sim_cpu = sim_pid.and_then(process_cpu_seconds);
    let _ = sim.kill().await;

    let fetched = pages.load(Ordering::Relaxed);
    let samples = samples.lock().unwrap().clone();
    // Per-second throughput from the regular samples (the final partial interval is excluded
    // here, and included in the whole-run window below).
    let regular = &samples[..samples.len() - 1];
    let per_sec: Vec<u64> = regular.windows(2).map(|w| w[1].1 - w[0].1).collect();
    // Steady window: the longest run of consecutive seconds at >= 50% of the peak rate.
    // Before it every host is fetching robots.txt and its root page; after it only hosts
    // with long Crawl-delays or pending retries remain.
    let peak = per_sec.iter().copied().max().unwrap_or(0);
    let (mut first, mut len, mut run_start) = (0usize, 0usize, None::<usize>);
    for (i, v) in per_sec.iter().chain(std::iter::once(&0)).enumerate() {
        match (*v * 2 >= peak && peak > 0, run_start) {
            (true, None) => run_start = Some(i),
            (false, Some(start)) => {
                if i - start > len {
                    (first, len) = (start, i - start);
                }
                run_start = None;
            }
            _ => {}
        }
    }
    let steady = &per_sec[first..first + len];
    let mut sorted = steady.to_vec();
    sorted.sort_unstable();
    let pct = |p: f64| {
        sorted
            .get(((sorted.len().max(1) as f64 - 1.0) * p).round() as usize)
            .copied()
            .unwrap_or(0)
    };
    // Rates over [samples[a], samples[b]] using the samples' own timestamps. The coordinator
    // adds busy time after each piece of work, so a window edge can be off by up to one piece
    // of work: the busy fraction is an estimate and can exceed 1.0 slightly.
    let window = |a: usize, b: usize| {
        let (s0, s1) = (samples[a], samples[b]);
        let secs = (s1.0 - s0.0).max(1e-9);
        json!({
            "from_seconds": round(s0.0),
            "to_seconds": round(s1.0),
            "seconds": round(secs),
            "pages": s1.1 - s0.1,
            "pages_per_second": round((s1.1 - s0.1) as f64 / secs),
            "crawler_cpu_cores": round((s1.2 - s0.2) / secs),
            "coordinator_busy_fraction_estimate": round((s1.3 - s0.3) as f64 / 1e6 / secs),
        })
    };
    let steady_window = if len > 0 {
        window(first, first + len)
    } else {
        json!(null)
    };
    let whole_window = if samples.len() > 1 {
        window(0, samples.len() - 1)
    } else {
        json!(null)
    };
    let t90 = samples
        .iter()
        .find(|v| v.1 * 10 >= fetched * 9)
        .map_or(elapsed, |v| round(v.0));
    let sim_resolved: Vec<String> = args.clone();

    let report = json!({
        "mode": "e2e",
        "machine": machine(),
        "params": { "hosts": hosts, "delay_ms": delay_ms, "concurrency": concurrency, "sim_args": sim_args, "max_depth": 2, "max_seconds": max_seconds },
        "provenance": {
            "rayap_revision": option_env!("RAYAP_REVISION").unwrap_or("unknown"),
            "simulator_command": format!("{} {}", sim_bin.display(), sim_resolved.join(" ")),
            "simulator_spec": sim_spec,
            "crawler_config": { "per_host_delay_ms": crawler_delay, "referee_delay_ms": delay_ms, "max_concurrency": concurrency, "request_timeout_ms": 30000, "retry_backoff_ms": 2000, "max_wait_ms": 60000, "max_attempts": 3, "max_depth": 2, "sqlite": "WAL, synchronous=NORMAL" },
            "started_unix": started_unix,
        },
        "finished": stats.as_ref().is_some_and(|s| s.finished),
        "wall_seconds": round(elapsed),
        "pages_ok": fetched,
        "events": kinds,
        "pages_per_second": {
            "overall": round(fetched as f64 / elapsed),
            "steady_window": steady_window,
            "steady_p50_1s": pct(0.5),
            "steady_p10_1s": pct(0.1),
            "peak_1s": peak,
        },
        "whole_run_window": whole_window,
        "seconds_to_90pct_of_pages": t90,
        "pages_per_second_series": per_sec,
        "politeness_bound_pages_per_second": round(f64::from(hosts) * 1000.0 / delay_ms as f64),
        "crawler_cpu_seconds": round(cpu1 - cpu0),
        "crawler_cpu_cores_avg": round((cpu1 - cpu0) / elapsed),
        "crawler_peak_rss_mib": round(rss),
        "simulator_cpu_seconds": sim_cpu.map(round),
        "simulator_cpu_cores_avg": sim_cpu.map(|c| round(c / elapsed)),
        "coordinator_busy_fraction_whole_run": stats.as_ref().map(|s| round(s.coordinator_busy.as_secs_f64() / elapsed)),
        "referee": referee,
        "stats": stats.map(|s| json!({ "requests": s.requests, "events": s.events, "bytes": s.bytes })),
    });
    println!("{}", serde_json::to_string_pretty(&report).unwrap());
}

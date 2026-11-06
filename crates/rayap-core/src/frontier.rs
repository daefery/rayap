//! The crawl frontier: one FIFO queue per host plus a min-heap of hosts keyed by the
//! earliest time each may be contacted again.
//!
//! A host is in the heap only while it is idle and has work, so `next` never returns a
//! second URL for a host that already has a request in flight, and a host that must wait
//! (politeness delay, Retry-After, backoff) never blocks the others. All time is passed in,
//! so the scheduling rules are testable without a clock.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};

use tokio::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub url: String,
    pub source: String,
    pub depth: u32,
    pub attempts: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    /// Contact `host` now. The host stays busy until `release`.
    Ready { host: String, task: Task },
    /// Nothing is ready; the earliest host becomes eligible at this instant.
    WaitUntil(Instant),
    /// No idle host has work (hosts with requests in flight may still have some).
    Idle,
}

struct Host {
    queue: VecDeque<Task>,
    next_allowed: Instant,
    delay: Duration,
    busy: bool,
    in_heap: bool,
}

pub struct Frontier {
    hosts: HashMap<String, Host>,
    ready: BinaryHeap<Reverse<(Instant, u64, String)>>,
    seq: u64,
    default_delay: Duration,
    pending: usize,
}

impl Frontier {
    pub fn new(default_delay: Duration) -> Self {
        Self {
            hosts: HashMap::new(),
            ready: BinaryHeap::new(),
            seq: 0,
            default_delay,
            pending: 0,
        }
    }

    /// Tasks waiting in queues (not counting requests in flight).
    pub fn pending(&self) -> usize {
        self.pending
    }

    pub fn push(&mut self, host: &str, task: Task, now: Instant) {
        self.host_mut(host, now).queue.push_back(task);
        self.pending += 1;
        self.schedule(host);
    }

    /// Puts a task back at the head of its host's queue (retries, robots.txt fetched first).
    pub fn push_front(&mut self, host: &str, task: Task, now: Instant) {
        self.host_mut(host, now).queue.push_front(task);
        self.pending += 1;
        self.schedule(host);
    }

    pub fn next(&mut self, now: Instant) -> Next {
        while let Some(Reverse((at, _, _))) = self.ready.peek() {
            if *at > now {
                return Next::WaitUntil(*at);
            }
            let Some(Reverse((_, _, host))) = self.ready.pop() else {
                break;
            };
            let state = self
                .hosts
                .get_mut(&host)
                .expect("heap only holds known hosts");
            state.in_heap = false;
            if let Some(task) = state.queue.pop_front() {
                state.busy = true;
                self.pending -= 1;
                return Next::Ready { host, task };
            }
        }
        Next::Idle
    }

    /// Marks the host idle again. Its next request may start `wait` after `now`.
    pub fn release(&mut self, host: &str, now: Instant, wait: Duration) {
        let state = self.host_mut(host, now);
        state.busy = false;
        state.next_allowed = now + wait;
        self.schedule(host);
    }

    pub fn delay(&self, host: &str) -> Duration {
        self.hosts.get(host).map_or(self.default_delay, |h| h.delay)
    }

    pub fn set_delay(&mut self, host: &str, delay: Duration, now: Instant) {
        self.host_mut(host, now).delay = delay;
    }

    fn host_mut(&mut self, host: &str, now: Instant) -> &mut Host {
        let default_delay = self.default_delay;
        self.hosts.entry(host.to_string()).or_insert_with(|| Host {
            queue: VecDeque::new(),
            next_allowed: now,
            delay: default_delay,
            busy: false,
            in_heap: false,
        })
    }

    fn schedule(&mut self, host: &str) {
        let Some(state) = self.hosts.get_mut(host) else {
            return;
        };
        if state.busy || state.in_heap || state.queue.is_empty() {
            return;
        }
        state.in_heap = true;
        self.seq += 1;
        self.ready
            .push(Reverse((state.next_allowed, self.seq, host.to_string())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(url: &str) -> Task {
        Task {
            url: url.into(),
            source: "s".into(),
            depth: 0,
            attempts: 0,
        }
    }

    fn ready_url(next: Next) -> String {
        match next {
            Next::Ready { task, .. } => task.url,
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn one_request_in_flight_per_host_and_delay_after_release() {
        let t0 = Instant::now();
        let mut f = Frontier::new(Duration::from_secs(2));
        f.push("a.org", task("a1"), t0);
        f.push("a.org", task("a2"), t0);

        assert_eq!(ready_url(f.next(t0)), "a1");
        assert_eq!(f.next(t0), Next::Idle, "a.org is busy");

        f.release("a.org", t0, f.delay("a.org"));
        assert_eq!(f.next(t0), Next::WaitUntil(t0 + Duration::from_secs(2)));
        assert_eq!(
            f.next(t0 + Duration::from_millis(1999)),
            Next::WaitUntil(t0 + Duration::from_secs(2))
        );
        assert_eq!(ready_url(f.next(t0 + Duration::from_secs(2))), "a2");
        assert_eq!(f.pending(), 0);
    }

    #[test]
    fn a_waiting_host_does_not_block_others() {
        let t0 = Instant::now();
        let mut f = Frontier::new(Duration::from_secs(1));
        f.push("slow.org", task("s1"), t0);
        f.push("slow.org", task("s2"), t0);
        f.push("fast.org", task("f1"), t0);

        assert_eq!(ready_url(f.next(t0)), "s1");
        f.release("slow.org", t0, Duration::from_secs(3600)); // e.g. Retry-After: 3600
        assert_eq!(ready_url(f.next(t0)), "f1");
        assert_eq!(f.next(t0), Next::WaitUntil(t0 + Duration::from_secs(3600)));
    }

    #[test]
    fn hosts_are_served_in_eligibility_order() {
        let t0 = Instant::now();
        let mut f = Frontier::new(Duration::ZERO);
        for host in ["a", "b", "c"] {
            f.push(host, task(host), t0);
        }
        let order: Vec<String> = (0..3).map(|_| ready_url(f.next(t0))).collect();
        assert_eq!(order, ["a", "b", "c"]);
    }

    #[test]
    fn push_front_retries_before_newer_work() {
        let t0 = Instant::now();
        let mut f = Frontier::new(Duration::ZERO);
        f.push("a", task("first"), t0);
        f.push("a", task("second"), t0);
        let Next::Ready { task: t, .. } = f.next(t0) else {
            panic!()
        };
        f.push_front("a", t, t0);
        assert_eq!(f.next(t0), Next::Idle, "still busy until released");
        f.release("a", t0, Duration::ZERO);
        assert_eq!(ready_url(f.next(t0)), "first");
    }

    #[test]
    fn per_host_delay_can_be_raised() {
        let t0 = Instant::now();
        let mut f = Frontier::new(Duration::from_secs(1));
        f.set_delay("a", Duration::from_secs(10), t0);
        assert_eq!(f.delay("a"), Duration::from_secs(10));
        assert_eq!(f.delay("other"), Duration::from_secs(1));
    }
}

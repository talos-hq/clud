//! Subscriptions and the rate-limit floor (#1743, phase 3) against a fake
//! GitHub whose ETag hashes the body and whose rate-limit headers the test
//! sets, on a fake clock that the subscription's own sleep advances.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use kernal_api::hash::Sha256Hasher as Sha256;

use super::super::{BrokerRead, GhBroker, ReadError, DEFAULT_TTL_MS, RUNS_TTL_MS};
use super::{WatchRequest, MAX_WATCH_MS};
use crate::gh_broker::upstream::{Response, Upstream, UpstreamRequest};

/// A change the fake applies once the clock reaches `at_ms`.
type Change = (u64, String, Vec<u8>);

#[derive(Default)]
struct Server {
    bodies: HashMap<String, Vec<u8>>,
    log: Vec<(String, Option<String>)>,
    /// `X-RateLimit-Remaining` of every response (limit 5000).
    remaining: u64,
    reset_s: u64,
    changes: Vec<Change>,
}

struct Fake {
    server: Arc<Mutex<Server>>,
    now: Arc<AtomicU64>,
}

impl Upstream for Fake {
    fn fetch(&self, request: &UpstreamRequest<'_>) -> Result<Response, String> {
        let mut server = self.server.lock().unwrap();
        let now = self.now.load(Ordering::SeqCst);
        let due: Vec<Change> = server
            .changes
            .iter()
            .filter(|c| c.0 <= now)
            .cloned()
            .collect();
        server.changes.retain(|c| c.0 > now);
        for (_, endpoint, body) in due {
            server.bodies.insert(endpoint, body);
        }
        server.log.push((
            request.endpoint.to_string(),
            request.if_none_match.map(str::to_string),
        ));
        let body = server
            .bodies
            .get(request.endpoint)
            .cloned()
            .ok_or("no such endpoint")?;
        let etag = format!("\"{:x}\"", Sha256::digest(&body).as_bytes()[0]);
        let headers = vec![
            ("Etag".to_string(), etag.clone()),
            ("X-RateLimit-Limit".to_string(), "5000".to_string()),
            (
                "X-RateLimit-Remaining".to_string(),
                server.remaining.to_string(),
            ),
            ("X-RateLimit-Reset".to_string(), server.reset_s.to_string()),
            ("X-RateLimit-Resource".to_string(), "core".to_string()),
        ];
        let (status, body) = if request.if_none_match == Some(etag.as_str()) {
            (304, Vec::new())
        } else {
            (200, body)
        };
        Ok(Response {
            status,
            headers,
            body: Arc::new(body),
        })
    }
}

const T0_MS: u64 = 1_790_000_000_000;
const RUN: &str = "repos/o/r/actions/runs/7";
const PR: &str = "repos/o/r/pulls/5";

struct World {
    _dir: kernal_api::platform::fs::TemporaryDirectory,
    broker: GhBroker,
    server: Arc<Mutex<Server>>,
    now: Arc<AtomicU64>,
}

fn world() -> World {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let now = Arc::new(AtomicU64::new(T0_MS));
    let server = Arc::new(Mutex::new(Server {
        bodies: HashMap::from([
            (
                RUN.to_string(),
                br#"{"id":7,"status":"in_progress"}"#.to_vec(),
            ),
            (PR.to_string(), br#"{"number":5,"state":"open"}"#.to_vec()),
        ]),
        remaining: 4_000,
        reset_s: T0_MS / 1000 + 3_600,
        ..Server::default()
    }));
    let clock = Arc::clone(&now);
    let slept = Arc::clone(&now);
    let broker = GhBroker::new(
        dir.path().join(crate::gh_broker::STORE_FILE),
        Box::new(Fake {
            server: Arc::clone(&server),
            now: Arc::clone(&now),
        }),
        Box::new(move || clock.load(Ordering::SeqCst)),
    )
    .with_sleep(Box::new(move |ms| {
        slept.fetch_add(ms, Ordering::SeqCst);
    }));
    World {
        _dir: dir,
        broker,
        server,
        now,
    }
}

impl World {
    fn request(&self, seen: Vec<Option<String>>, wait_ms: u64) -> WatchRequest {
        WatchRequest {
            gh: "/usr/bin/gh".into(),
            hostname: None,
            env: vec![],
            session_id: None,
            endpoints: vec![RUN.into(), PR.into()],
            seen,
            wait_ms,
        }
    }

    fn requests(&self) -> usize {
        self.server.lock().unwrap().log.len()
    }

    fn elapsed(&self) -> u64 {
        self.now.load(Ordering::SeqCst) - T0_MS
    }

    fn read(&self, endpoint: &str, interactive: bool) -> (Result<Response, ReadError>, bool) {
        let read = BrokerRead {
            gh: Path::new("/usr/bin/gh"),
            endpoint,
            hostname: None,
            env: &[],
            session_id: None,
            fresh: false,
            interactive,
            quiet: false,
        };
        let (result, stale) = self.broker.read_marked(&read);
        (result, stale.is_some())
    }

    fn outcomes(&self) -> Vec<String> {
        self.broker
            .ledger()
            .unwrap()
            .into_iter()
            .map(|e| e.outcome)
            .collect()
    }
}

#[test]
fn a_baseline_returns_at_once_and_a_quiet_wait_costs_nothing_until_the_ttl() {
    let w = world();
    let gh = Path::new("/usr/bin/gh");
    let baseline = w.broker.watch(gh, &w.request(vec![], 50_000));
    assert_eq!(baseline.changed, [0, 1]);
    assert_eq!(w.elapsed(), 0);
    assert_eq!(w.requests(), 2);
    // Nothing changes: the run (30 s TTL) is revalidated for a free 304 once
    // per TTL, the PR (60 s) not at all inside a 50 s wait.
    let reply = w
        .broker
        .watch(gh, &w.request(baseline.digests.clone(), 50_000));
    assert!(reply.changed.is_empty());
    assert_eq!(reply.digests, baseline.digests);
    assert_eq!(w.elapsed(), 50_000);
    let log = w.server.lock().unwrap().log.clone();
    assert_eq!(log.len(), 3);
    assert_eq!(log[2].0, RUN);
    assert!(log[2].1.is_some(), "a conditional revalidation");
    // Cache hits of a blocked subscription stay out of the ledger.
    assert_eq!(w.outcomes(), ["full", "full", "304"]);
}

#[test]
fn a_change_wakes_the_waiter_at_the_next_refresh() {
    let w = world();
    let gh = Path::new("/usr/bin/gh");
    let baseline = w.broker.watch(gh, &w.request(vec![], 50_000));
    w.server.lock().unwrap().changes.push((
        T0_MS + 10_000,
        RUN.to_string(),
        br#"{"id":7,"status":"completed"}"#.to_vec(),
    ));
    let reply = w
        .broker
        .watch(gh, &w.request(baseline.digests.clone(), 50_000));
    assert_eq!(reply.changed, [0]);
    // Seen at the run's first refresh after the change: its TTL.
    assert_eq!(w.elapsed(), RUNS_TTL_MS);
    // The waiter's own read of the same key is now a cache hit.
    let before = w.requests();
    let (body, _) = w.read(RUN, false);
    assert!(String::from_utf8_lossy(&body.unwrap().body).contains("completed"));
    assert_eq!(w.requests(), before);
}

#[test]
fn a_wait_is_capped_and_a_failed_read_never_counts_as_a_change() {
    let w = world();
    let gh = Path::new("/usr/bin/gh");
    let baseline = w.broker.watch(gh, &w.request(vec![], 50_000));
    w.server.lock().unwrap().bodies.remove(PR);
    let reply = w
        .broker
        .watch(gh, &w.request(baseline.digests.clone(), 10 * MAX_WATCH_MS));
    assert_eq!(w.elapsed(), MAX_WATCH_MS, "capped");
    assert!(reply.changed.is_empty());
    // The PR's refresh (60 s TTL) fails in the next call: `null`, which is
    // not a change.
    let reply = w
        .broker
        .watch(gh, &w.request(baseline.digests.clone(), MAX_WATCH_MS));
    assert!(w.elapsed() > DEFAULT_TTL_MS);
    assert!(reply.changed.is_empty());
    assert_eq!(reply.digests[1], None);
}

/// Below 10% of the limit in the same window (it resets an hour after T0).
fn below_floor(w: &World) {
    w.server.lock().unwrap().remaining = 400;
}

#[test]
fn below_the_floor_background_reads_are_served_stale_and_interactive_reads_proceed() {
    let w = world();
    below_floor(&w);
    // The first fetch teaches the broker the window.
    assert!(w.read(RUN, false).0.is_ok());
    w.now.fetch_add(RUNS_TTL_MS, Ordering::SeqCst);
    let before = w.requests();
    let (stale, marked) = w.read(RUN, false);
    assert!(marked, "a stored copy is marked stale");
    assert_eq!(
        stale.unwrap().body.as_slice(),
        br#"{"id":7,"status":"in_progress"}"#
    );
    assert_eq!(w.requests(), before, "deferred: no upstream request");
    // Nothing stored: the shim runs the real gh rather than invent a body.
    assert_eq!(w.read(PR, false).0.unwrap_err(), ReadError::Deferred);
    // A terminal on stdin: a person is waiting, the read goes upstream.
    let (fresh, marked) = w.read(RUN, true);
    assert!(fresh.is_ok() && !marked);
    assert_eq!(w.requests(), before + 1);
    assert_eq!(w.outcomes(), ["full", "deferred", "deferred", "304"]);
}

#[test]
fn a_deferred_subscription_sleeps_to_the_reset_and_says_so() {
    let w = world();
    let gh = Path::new("/usr/bin/gh");
    let baseline = w.broker.watch(gh, &w.request(vec![], 50_000));
    below_floor(&w);
    // One more fetch learns the low window.
    w.now.fetch_add(RUNS_TTL_MS, Ordering::SeqCst);
    assert!(w.read(RUN, true).0.is_ok());
    w.now.fetch_add(RUNS_TTL_MS, Ordering::SeqCst);
    let start = w.elapsed();
    let reply = w
        .broker
        .watch(gh, &w.request(baseline.digests.clone(), 50_000));
    assert!(reply.changed.is_empty(), "a deferral is never a change");
    assert!(reply.deferred_until_s.is_some());
    // It slept to its deadline in one step instead of ticking.
    assert_eq!(w.elapsed() - start, 50_000);
    let deferred = w.outcomes().iter().filter(|o| *o == "deferred").count();
    assert_eq!(
        deferred, 2,
        "one ledger row per deferred key, not one per tick"
    );
}

#[test]
fn the_reserve_can_turn_the_floor_off() {
    let w = world();
    let broker = GhBroker::new(
        w._dir.path().join("other.redb"),
        Box::new(Fake {
            server: Arc::clone(&w.server),
            now: Arc::clone(&w.now),
        }),
        {
            let now = Arc::clone(&w.now);
            Box::new(move || now.load(Ordering::SeqCst))
        },
    )
    .with_reserve(Box::new(|| 0));
    below_floor(&w);
    let read = BrokerRead {
        gh: Path::new("/usr/bin/gh"),
        endpoint: RUN,
        hostname: None,
        env: &[],
        session_id: None,
        fresh: true,
        interactive: false,
        quiet: false,
    };
    broker.read(&read).unwrap();
    let before = w.requests();
    broker.read(&read).unwrap();
    assert_eq!(w.requests(), before + 1, "reserve 0: never deferred");
}

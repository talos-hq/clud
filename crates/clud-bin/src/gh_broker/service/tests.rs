use super::*;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

/// Records every upstream request; answers 304 to a matching
/// `If-None-Match`, else `status` with an ETag'd body.
struct FakeUpstream {
    calls: Arc<AtomicUsize>,
    conditional: Arc<Mutex<Vec<Option<String>>>>,
    status: u16,
    gate: Option<Mutex<mpsc::Receiver<()>>>,
}

impl Upstream for FakeUpstream {
    fn fetch(&self, request: &UpstreamRequest<'_>) -> Result<Response, String> {
        if let Some(gate) = &self.gate {
            let _ = gate.lock().unwrap().recv_timeout(Duration::from_secs(10));
        }
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.conditional
            .lock()
            .unwrap()
            .push(request.if_none_match.map(str::to_string));
        let headers = vec![
            ("Content-Type".to_string(), "application/json".to_string()),
            ("Etag".to_string(), "\"v1\"".to_string()),
            ("X-Ratelimit-Remaining".to_string(), "4321".to_string()),
        ];
        if request.if_none_match == Some("\"v1\"") {
            return Ok(Response {
                status: 304,
                headers,
                body: Arc::new(Vec::new()),
            });
        }
        Ok(Response {
            status: self.status,
            headers,
            body: Arc::new(format!("{{\"endpoint\":\"{}\"}}", request.endpoint).into_bytes()),
        })
    }
}

struct World {
    _dir: kernal_api::platform::fs::TemporaryDirectory,
    broker: Arc<GhBroker>,
    calls: Arc<AtomicUsize>,
    conditional: Arc<Mutex<Vec<Option<String>>>>,
    now: Arc<AtomicU64>,
}

fn world_with(status: u16, gate: Option<mpsc::Receiver<()>>) -> World {
    let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let conditional = Arc::new(Mutex::new(Vec::new()));
    let now = Arc::new(AtomicU64::new(1_000_000));
    let clock_now = Arc::clone(&now);
    let broker = GhBroker::new(
        dir.path().join(crate::gh_broker::STORE_FILE),
        Box::new(FakeUpstream {
            calls: Arc::clone(&calls),
            conditional: Arc::clone(&conditional),
            status,
            gate: gate.map(Mutex::new),
        }),
        Box::new(move || clock_now.load(Ordering::SeqCst)),
    );
    World {
        _dir: dir,
        broker: Arc::new(broker),
        calls,
        conditional,
        now,
    }
}

fn world() -> World {
    world_with(200, None)
}

const GH: &str = "/usr/bin/gh";

fn read<'a>(endpoint: &'a str, env: &'a [(String, String)]) -> BrokerRead<'a> {
    BrokerRead {
        gh: Path::new(GH),
        endpoint,
        hostname: None,
        env,
        session_id: Some("s1"),
        fresh: false,
        interactive: false,
        quiet: false,
    }
}

fn outcomes(world: &World) -> Vec<String> {
    world
        .broker
        .ledger()
        .unwrap()
        .into_iter()
        .map(|e| e.outcome)
        .collect()
}

#[test]
fn a_second_read_within_the_ttl_makes_no_upstream_request() {
    let w = world();
    let first = w.broker.read(&read("repos/o/r", &[])).unwrap();
    w.now.fetch_add(DEFAULT_TTL_MS - 1, Ordering::SeqCst);
    let second = w.broker.read(&read("/repos/o/r", &[])).unwrap();
    assert_eq!(w.calls.load(Ordering::SeqCst), 1);
    assert_eq!(first, second);
    assert_eq!(outcomes(&w), ["full", "cache"]);
    let ledger = w.broker.ledger().unwrap();
    assert_eq!(ledger[0].rate_remaining, Some(4321));
    assert_eq!(ledger[0].session_id.as_deref(), Some("s1"));
    assert_eq!(ledger[0].key, "/repos/o/r");
    assert_eq!(
        (ledger[0].upstream_requests, ledger[1].upstream_requests),
        (1, 0)
    );
}

#[test]
fn after_the_ttl_a_304_revalidation_serves_the_cached_body() {
    let w = world();
    let first = w
        .broker
        .read(&read("repos/o/r/actions/runs/7", &[]))
        .unwrap();
    w.now.fetch_add(RUNS_TTL_MS, Ordering::SeqCst);
    let second = w
        .broker
        .read(&read("repos/o/r/actions/runs/7", &[]))
        .unwrap();
    assert_eq!(first.body, second.body);
    assert_eq!(second.status, 200);
    assert_eq!(
        *w.conditional.lock().unwrap(),
        [None, Some("\"v1\"".to_string())]
    );
    // The revalidation restarted the TTL.
    w.now.fetch_add(RUNS_TTL_MS - 1, Ordering::SeqCst);
    w.broker
        .read(&read("repos/o/r/actions/runs/7", &[]))
        .unwrap();
    assert_eq!(w.calls.load(Ordering::SeqCst), 2);
    assert_eq!(outcomes(&w), ["full", "304", "cache"]);
}

#[test]
fn concurrent_reads_of_one_key_share_one_upstream_request() {
    let (release, gate) = mpsc::channel();
    let w = world_with(200, Some(gate));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let broker = Arc::clone(&w.broker);
            std::thread::spawn(move || broker.read(&read("repos/o/r/pulls/1", &[])).unwrap())
        })
        .collect();
    std::thread::sleep(Duration::from_millis(500));
    release.send(()).unwrap();
    let bodies: Vec<_> = threads
        .into_iter()
        .map(|t| t.join().unwrap().body)
        .collect();
    assert_eq!(w.calls.load(Ordering::SeqCst), 1);
    assert!(bodies.windows(2).all(|pair| pair[0] == pair[1]));
    let mut seen = outcomes(&w);
    seen.sort();
    assert_eq!(
        seen,
        ["cache"; 7]
            .iter()
            .chain(&["full"])
            .cloned()
            .collect::<Vec<_>>()
    );
}

#[test]
fn invalidation_forces_a_revalidation_inside_the_ttl() {
    let w = world();
    w.broker.read(&read("repos/o/r/issues/3", &[])).unwrap();
    w.now.fetch_add(10, Ordering::SeqCst);
    w.broker.invalidate().unwrap();
    w.now.fetch_add(10, Ordering::SeqCst);
    w.broker.read(&read("repos/o/r/issues/3", &[])).unwrap();
    assert_eq!(w.calls.load(Ordering::SeqCst), 2);
    assert_eq!(outcomes(&w), ["full", "304"]);
}

#[test]
fn fresh_skips_the_ttl_but_still_revalidates() {
    let w = world();
    w.broker.read(&read("repos/o/r", &[])).unwrap();
    let mut fresh = read("repos/o/r", &[]);
    fresh.fresh = true;
    w.broker.read(&fresh).unwrap();
    assert_eq!(outcomes(&w), ["full", "304"]);
}

#[test]
fn identities_never_share_a_cached_body() {
    let w = world();
    let alice = [("GH_TOKEN".to_string(), "alice".to_string())];
    let bob = [("GH_TOKEN".to_string(), "bob".to_string())];
    w.broker.read(&read("repos/o/r", &alice)).unwrap();
    w.broker.read(&read("repos/o/r", &bob)).unwrap();
    w.broker.read(&read("repos/o/r", &alice)).unwrap();
    assert_eq!(w.calls.load(Ordering::SeqCst), 2);
    assert_eq!(outcomes(&w), ["full", "full", "cache"]);
}

#[test]
fn upstream_errors_are_not_cached_and_ask_for_passthrough() {
    let w = world_with(404, None);
    assert_eq!(
        w.broker.read(&read("repos/o/missing", &[])),
        Err(ReadError::Passthrough(404))
    );
    assert_eq!(
        w.broker.read(&read("repos/o/missing", &[])),
        Err(ReadError::Passthrough(404))
    );
    assert_eq!(w.calls.load(Ordering::SeqCst), 2);
    assert_eq!(outcomes(&w), ["passthrough", "passthrough"]);
}

#[test]
fn ttl_is_shorter_for_runs_and_jobs() {
    assert_eq!(ttl_ms("repos/o/r/actions/runs"), RUNS_TTL_MS);
    assert_eq!(
        ttl_ms("repos/o/r/actions/runs/1/jobs?per_page=100"),
        RUNS_TTL_MS
    );
    assert_eq!(ttl_ms("/repos/o/r/actions/jobs/9"), RUNS_TTL_MS);
    assert_eq!(ttl_ms("repos/o/r/pulls/1"), DEFAULT_TTL_MS);
    assert_eq!(ttl_ms("repos/o/r/commits/abc/check-runs"), DEFAULT_TTL_MS);
}

#[test]
fn http_handler_refuses_a_gh_target_that_is_not_a_real_executable() {
    let w = world();
    let exe = std::env::current_exe().unwrap();
    for gh in [
        "relative/gh".to_string(),
        exe.to_string_lossy().into_owned(),
    ] {
        let body = serde_json::to_vec(&ReadRequest {
            gh,
            endpoint: "repos/o/r".into(),
            hostname: None,
            env: vec![],
            session_id: None,
            fresh: false,
            interactive: false,
        })
        .unwrap();
        assert_eq!(w.broker.handle_http(&body, &exe, None).0, 400);
    }
    assert_eq!(w.broker.handle_http(b"not json", &exe, None).0, 400);
    if crate::platform::host::UNIX {
        // A real executable that is not named gh is refused too.
        let sh = serde_json::to_vec(&ReadRequest {
            gh: "/bin/sh".into(),
            endpoint: "repos/o/r".into(),
            hostname: None,
            env: vec![],
            session_id: None,
            fresh: false,
            interactive: false,
        })
        .unwrap();
        assert_eq!(w.broker.handle_http(&sh, &exe, None).0, 400);
    }
    assert_eq!(w.calls.load(Ordering::SeqCst), 0);
}

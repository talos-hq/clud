//! Merged collections, listings and targeted invalidation (#1743), against a
//! fake GitHub that answers each query (`branch`, `per_page`, `page`,
//! `If-None-Match`) the way the REST API does. Every merged answer is
//! compared with the fake's own full answer to the caller's exact URL.
//!
//! The fake's ETag is a hash of the body, as GitHub's is, so a `304` proves
//! the page is byte-identical to the stored one: deletions, reactions and
//! reruns that do not move `updated_at` all change it.

use super::merged::UNMERGEABLE_RETRY_MS;
use super::*;
use crate::gh_broker::collection::{format_ts, parse_ts, split_query};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone)]
struct Comment {
    id: u64,
    created: i64,
    updated: i64,
    body: String,
    /// Reaction count: GitHub changes it without moving `updated_at`.
    reactions: u32,
}

#[derive(Clone)]
struct Run {
    id: u64,
    created: i64,
    updated: i64,
    status: &'static str,
    branch: &'static str,
    attempt: u32,
}

#[derive(Clone)]
struct Step {
    status: &'static str,
    completed_at: Option<i64>,
}

/// Mutates the server when a request for a matching endpoint arrives,
/// before it is answered: a change that lands in the middle of a refresh.
type Hook = Box<dyn FnMut(&mut Server, &str) + Send>;

#[derive(Default)]
struct Server {
    comments: Vec<Comment>,
    runs: Vec<Run>,
    jobs: Vec<Step>,
    checks: Vec<Step>,
    /// Bodies served verbatim for an exact endpoint.
    raw: HashMap<String, Vec<u8>>,
    /// Every upstream request: endpoint and `If-None-Match`.
    log: Vec<(String, Option<String>)>,
    hook: Option<Hook>,
}

fn page_of<T: Clone>(items: &[T], params: &HashMap<&str, &str>) -> (Vec<T>, bool) {
    let per_page: usize = params.get("per_page").map_or(30, |v| v.parse().unwrap());
    let page: usize = params.get("page").map_or(1, |v| v.parse().unwrap());
    let start = (page - 1) * per_page;
    let end = (start + per_page).min(items.len());
    let shown = items.get(start..end).unwrap_or_default().to_vec();
    (shown, end < items.len())
}

fn steps_json(steps: &[Step]) -> String {
    steps
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let at = s
                .completed_at
                .map_or("null".to_string(), |at| format!("\"{}\"", format_ts(at)));
            format!(
                r#"{{"id":{i},"status":"{}","completed_at":{at}}}"#,
                s.status
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

impl Server {
    /// The REST answer to `endpoint`: status, body, `Link` next.
    fn respond(&self, endpoint: &str) -> (u16, Vec<u8>, bool) {
        if let Some(body) = self.raw.get(endpoint) {
            return (200, body.clone(), false);
        }
        let (path, pairs) = split_query(endpoint).unwrap();
        let params: HashMap<&str, &str> = pairs.into_iter().collect();
        let sha = "0123456789abcdef0123456789abcdef01234567";
        match path {
            "repos/o/r/issues/5/comments" => {
                let mut items: Vec<&Comment> = self.comments.iter().collect();
                items.sort_by_key(|c| c.id);
                let (shown, more) = page_of(&items, &params);
                let body = shown
                    .iter()
                    .map(|c| {
                        format!(
                            r#"{{"id":{},"node_id":"IC_{}","body":"{}","created_at":"{}","updated_at":"{}","reactions":{{"total_count":{}}}}}"#,
                            c.id,
                            c.id,
                            c.body,
                            format_ts(c.created),
                            format_ts(c.updated),
                            c.reactions
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                (200, format!("[{body}]").into_bytes(), more)
            }
            "repos/o/r/actions/runs" => {
                let mut items: Vec<&Run> = self
                    .runs
                    .iter()
                    .filter(|r| params.get("branch").is_none_or(|b| r.branch == *b))
                    .collect();
                items.sort_by(|a, b| b.created.cmp(&a.created).then(b.id.cmp(&a.id)));
                let (shown, more) = page_of(&items, &params);
                let body = shown
                    .iter()
                    .map(|r| {
                        format!(
                            r#"{{"id":{},"head_branch":"{}","run_attempt":{},"status":"{}","created_at":"{}","updated_at":"{}"}}"#,
                            r.id,
                            r.branch,
                            r.attempt,
                            r.status,
                            format_ts(r.created),
                            format_ts(r.updated)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                let body = format!(
                    r#"{{"total_count":{},"workflow_runs":[{body}]}}"#,
                    items.len()
                );
                (200, body.into_bytes(), more)
            }
            "repos/o/r/actions/runs/7/jobs" => {
                let body = format!(
                    r#"{{"total_count":{},"jobs":[{}]}}"#,
                    self.jobs.len(),
                    steps_json(&self.jobs)
                );
                (200, body.into_bytes(), false)
            }
            _ if path == format!("repos/o/r/commits/{sha}/check-runs") => {
                let body = format!(
                    r#"{{"total_count":{},"check_runs":[{}]}}"#,
                    self.checks.len(),
                    steps_json(&self.checks)
                );
                (200, body.into_bytes(), false)
            }
            _ => (404, b"{\"message\":\"Not Found\"}".to_vec(), false),
        }
    }
}

struct FakeGitHub(Arc<Mutex<Server>>);

impl Upstream for FakeGitHub {
    fn fetch(&self, request: &UpstreamRequest<'_>) -> Result<Response, String> {
        let mut server = self.0.lock().unwrap();
        server.log.push((
            request.endpoint.to_string(),
            request.if_none_match.map(str::to_string),
        ));
        if let Some(mut hook) = server.hook.take() {
            hook(&mut server, request.endpoint);
            server.hook = Some(hook);
        }
        let (status, body, more) = server.respond(request.endpoint);
        let digest = Sha256::digest(&body);
        let etag = format!(
            "\"{:02x}{:02x}{:02x}{:02x}\"",
            digest.as_bytes()[0],
            digest.as_bytes()[1],
            digest.as_bytes()[2],
            digest.as_bytes()[3]
        );
        let mut headers = vec![
            (
                "Content-Type".to_string(),
                "application/json; charset=utf-8".to_string(),
            ),
            ("Etag".to_string(), etag.clone()),
            ("X-Ratelimit-Remaining".to_string(), "4000".to_string()),
        ];
        if more {
            headers.push((
                "Link".to_string(),
                "<https://api.github.com/x?page=2>; rel=\"next\"".to_string(),
            ));
        }
        if status == 200 && request.if_none_match == Some(etag.as_str()) {
            return Ok(Response {
                status: 304,
                headers,
                body: Arc::new(Vec::new()),
            });
        }
        Ok(Response {
            status,
            headers,
            body: Arc::new(body),
        })
    }
}

struct World {
    _dir: tempfile::TempDir,
    broker: GhBroker,
    server: Arc<Mutex<Server>>,
    now: Arc<AtomicU64>,
}

fn ts(text: &str) -> i64 {
    parse_ts(text).unwrap()
}

fn world(server: Server) -> World {
    let dir = tempfile::tempdir().unwrap();
    let server = Arc::new(Mutex::new(server));
    let now = Arc::new(AtomicU64::new(
        u64::try_from(ts("2026-10-02T12:00:00Z")).unwrap() * 1000,
    ));
    let clock_now = Arc::clone(&now);
    let broker = GhBroker::new(
        dir.path().join(crate::gh_broker::STORE_FILE),
        Box::new(FakeGitHub(Arc::clone(&server))),
        Box::new(move || clock_now.load(Ordering::SeqCst)),
    );
    World {
        _dir: dir,
        broker,
        server,
        now,
    }
}

impl World {
    fn read(&self, endpoint: &str) -> Vec<u8> {
        let read = BrokerRead {
            gh: Path::new("/usr/bin/gh"),
            endpoint,
            hostname: None,
            env: &[],
            session_id: None,
            fresh: false,
            interactive: false,
            quiet: false,
        };
        self.broker.read(&read).unwrap().body.to_vec()
    }

    /// What GitHub itself returns for `endpoint` right now.
    fn full(&self, endpoint: &str) -> Vec<u8> {
        self.server.lock().unwrap().respond(endpoint).1
    }

    fn log(&self) -> Vec<(String, Option<String>)> {
        self.server.lock().unwrap().log.clone()
    }

    fn requests(&self) -> Vec<String> {
        self.log()
            .into_iter()
            .map(|(endpoint, _)| endpoint)
            .collect()
    }

    fn advance(&self, ms: u64) {
        self.now.fetch_add(ms, Ordering::SeqCst);
    }

    fn ledger(&self) -> Vec<(String, u32, Option<u32>)> {
        self.broker
            .ledger()
            .unwrap()
            .into_iter()
            .map(|e| (e.outcome, e.upstream_requests, e.changed))
            .collect()
    }

    fn outcomes(&self) -> Vec<String> {
        self.ledger()
            .into_iter()
            .map(|(outcome, _, _)| outcome)
            .collect()
    }

    fn server(&self) -> std::sync::MutexGuard<'_, Server> {
        self.server.lock().unwrap()
    }
}

fn comment(id: u64, created: &str) -> Comment {
    Comment {
        id,
        created: ts(created),
        updated: ts(created),
        body: format!("c{id}"),
        reactions: 0,
    }
}

fn three_comments() -> Server {
    Server {
        comments: vec![
            comment(1, "2026-10-02T10:00:00Z"),
            comment(2, "2026-10-02T10:01:00Z"),
            comment(3, "2026-10-02T10:02:00Z"),
        ],
        ..Server::default()
    }
}

/// `n` comments one minute apart, ids 1..=n.
fn many_comments(n: u64) -> Server {
    let mut server = Server::default();
    for id in 1..=n {
        server.comments.push(comment(
            id,
            &format_ts(ts("2026-10-01T00:00:00Z") + id as i64 * 60),
        ));
    }
    server
}

const COMMENTS: &str = "repos/o/r/issues/5/comments";
const SEED: &str = "repos/o/r/issues/5/comments?per_page=100";
const PAGE2: &str = "repos/o/r/issues/5/comments?per_page=100&page=2";

fn entry(outcome: &str, requests: u32, changed: Option<u32>) -> (String, u32, Option<u32>) {
    (outcome.to_string(), requests, changed)
}

#[test]
fn a_comment_reread_revalidates_its_page_and_equals_a_full_fetch() {
    let w = world(three_comments());
    assert_eq!(w.read(COMMENTS), w.full(COMMENTS));
    {
        let mut server = w.server();
        server.comments.push(comment(4, "2026-10-02T11:00:00Z"));
        server.comments[1].updated = ts("2026-10-02T11:30:00Z");
        server.comments[1].body = "edited".into();
    }
    w.advance(DEFAULT_TTL_MS);
    let merged = w.read(COMMENTS);
    assert_eq!(merged, w.full(COMMENTS));
    assert!(String::from_utf8_lossy(&merged).contains("edited"));
    // The refresh re-sends the seed with its ETag; the 200 is the whole
    // truth for a one-page collection.
    let log = w.log();
    assert_eq!(w.requests(), [SEED, SEED]);
    assert!(log[0].1.is_none() && log[1].1.is_some());
    // Within the TTL, any page size is cut from the merged membership.
    for endpoint in [
        "repos/o/r/issues/5/comments?per_page=2",
        "/repos/o/r/issues/5/comments?page=1&per_page=100",
    ] {
        assert_eq!(w.read(endpoint), w.full(endpoint.trim_start_matches('/')));
    }
    assert_eq!(w.requests().len(), 2);
    assert_eq!(
        w.ledger(),
        [
            entry("full", 1, None),
            entry("incremental", 1, Some(2)),
            entry("cache", 0, None),
            entry("cache", 0, None),
        ]
    );
}

#[test]
fn a_quiet_collection_costs_only_free_304s_even_right_after_a_change() {
    let w = world(three_comments());
    w.read(COMMENTS);
    w.server().comments.push(comment(4, "2026-10-02T11:00:00Z"));
    w.advance(DEFAULT_TTL_MS);
    w.read(COMMENTS);
    w.advance(DEFAULT_TTL_MS);
    assert_eq!(w.read(COMMENTS), w.full(COMMENTS));
    // The refresh after a change carries the ETag of the page it got, so
    // nothing new costs a 304, not another charged 200.
    assert!(w.log()[2].1.is_some());
    assert_eq!(
        w.ledger(),
        [
            entry("full", 1, None),
            entry("incremental", 1, Some(1)),
            entry("304", 1, Some(0)),
        ]
    );
}

#[test]
fn a_reaction_that_does_not_move_updated_at_is_seen_on_the_next_refresh() {
    let w = world(three_comments());
    w.read(COMMENTS);
    w.server().comments[0].reactions = 1;
    w.advance(DEFAULT_TTL_MS);
    let merged = w.read(COMMENTS);
    assert_eq!(merged, w.full(COMMENTS));
    assert!(String::from_utf8_lossy(&merged).contains(r#""reactions":{"total_count":1}"#));
    assert_eq!(w.ledger()[1], entry("incremental", 1, Some(1)));
}

#[test]
fn a_deleted_comment_drops_out_on_the_next_refresh() {
    let w = world(three_comments());
    w.read(COMMENTS);
    w.server().comments.remove(1);
    w.advance(DEFAULT_TTL_MS);
    assert_eq!(w.read(COMMENTS), w.full(COMMENTS));
    let removed = w.broker.ledger().unwrap()[1].removed;
    assert_eq!(removed, Some(1));
    // Brought back byte for byte: the next refresh is a free 304.
    w.advance(DEFAULT_TTL_MS);
    assert_eq!(w.read(COMMENTS), w.full(COMMENTS));
    assert_eq!(w.outcomes(), ["full", "incremental", "304"]);
}

#[test]
fn a_write_that_names_the_collection_refreshes_it_inside_the_ttl() {
    let w = world(three_comments());
    w.read(COMMENTS);
    w.server().comments.remove(0);
    w.broker
        .invalidate_tags(&["o/r#num:6".to_string()])
        .unwrap();
    w.advance(1);
    assert_ne!(w.read(COMMENTS), w.full(COMMENTS), "another issue's write");
    w.broker.invalidate_tags(&["*#num:5".to_string()]).unwrap();
    w.advance(1);
    assert_eq!(w.read(COMMENTS), w.full(COMMENTS));
    assert_eq!(w.outcomes(), ["full", "cache", "incremental"]);
}

#[test]
fn every_page_of_a_multi_page_collection_is_revalidated_for_free() {
    let w = world(many_comments(150));
    for endpoint in [
        "repos/o/r/issues/5/comments?per_page=100",
        COMMENTS,
        "repos/o/r/issues/5/comments?per_page=7&page=1",
    ] {
        assert_eq!(w.read(endpoint), w.full(endpoint), "{endpoint}");
    }
    // The seed pages through, then re-checks the pages before the last, so
    // a deletion between two page requests cannot drop a comment.
    assert_eq!(w.requests(), [SEED, PAGE2, SEED]);
    // Unchanged: one free 304 per page.
    w.advance(DEFAULT_TTL_MS);
    assert_eq!(w.read(COMMENTS), w.full(COMMENTS));
    let log = w.log();
    assert_eq!(w.requests()[3..], [SEED, PAGE2]);
    assert!(log[3].1.is_some() && log[4].1.is_some());
    assert_eq!(w.ledger()[3], entry("304", 2, Some(0)));
}

#[test]
fn a_reaction_or_deletion_on_any_page_is_seen_on_the_next_refresh() {
    let endpoint = "repos/o/r/issues/5/comments?per_page=100";
    let w = world(many_comments(150));
    w.read(endpoint);
    // A reaction on page 2 only: page 1 is a 304, page 2 a 200.
    w.server().comments[120].reactions = 3;
    w.advance(DEFAULT_TTL_MS);
    assert_eq!(w.read(endpoint), w.full(endpoint));
    // A deletion on page 1 shifts page 2 too.
    w.server().comments.remove(10);
    w.advance(DEFAULT_TTL_MS);
    assert_eq!(w.read(endpoint), w.full(endpoint));
    w.advance(DEFAULT_TTL_MS);
    assert_eq!(w.read(endpoint), w.full(endpoint));
    assert_eq!(w.outcomes(), ["full", "incremental", "incremental", "304"]);
}

#[test]
fn a_full_last_page_is_followed_so_a_new_page_is_not_missed() {
    let w = world(many_comments(100));
    assert_eq!(w.read(SEED), w.full(SEED));
    // Page 1 is full, so the seed asks for page 2 (empty) to know it is
    // last, then re-checks page 1.
    assert_eq!(w.requests(), [SEED, PAGE2, SEED]);
    // A new comment opens page 2 and leaves page 1 byte-identical: a 304
    // on a full page never ends the pass.
    w.server()
        .comments
        .push(comment(101, "2026-10-02T11:00:00Z"));
    w.advance(DEFAULT_TTL_MS);
    assert_eq!(w.read(SEED), w.full(SEED));
    assert_eq!(w.requests()[3..], [SEED, PAGE2, SEED]);
    assert_eq!(w.ledger()[1], entry("incremental", 3, Some(1)));
}

#[test]
fn a_deletion_in_the_middle_of_a_pass_is_caught_before_it_is_served() {
    let w = world(many_comments(150));
    w.read(SEED);
    w.server().comments[120].reactions = 1;
    // While the refresh asks for page 2, comment 11 (on page 1, already
    // revalidated) is deleted: page 2 shifts and comment 101 moves to
    // page 1. Without the re-check the answer would still hold comment 11
    // and miss comment 101.
    w.server().hook = Some(Box::new(|server: &mut Server, endpoint: &str| {
        if endpoint.ends_with("page=2") && server.comments[10].id == 11 {
            server.comments.remove(10);
        }
    }));
    w.advance(DEFAULT_TTL_MS);
    assert_eq!(w.read(SEED), w.full(SEED));
    // The re-check of page 1 fails, so the pass runs again.
    assert_eq!(w.requests()[3..], [SEED, PAGE2, SEED, SEED, PAGE2, SEED]);
    assert_eq!(w.outcomes(), ["full", "incremental"]);
}

#[test]
fn a_collection_that_keeps_changing_under_both_passes_reads_the_exact_url() {
    let w = world(many_comments(150));
    w.read(SEED);
    w.server().comments[120].reactions = 1;
    // Every request for page 2 deletes the first comment left on page 1,
    // so neither pass fits together.
    w.server().hook = Some(Box::new(|server: &mut Server, endpoint: &str| {
        if endpoint.ends_with("page=2") {
            server.comments.remove(0);
        }
    }));
    w.advance(DEFAULT_TTL_MS);
    assert_eq!(w.read(SEED), w.full(SEED));
    w.server().hook = None;
    // The answer came from the exact URL, and the stored pages were kept:
    // the next refresh revalidates them instead of starting over.
    let requests = w.requests();
    assert_eq!(requests[3..9], [SEED, PAGE2, SEED, SEED, PAGE2, SEED]);
    assert_eq!(requests[9], SEED);
    w.advance(DEFAULT_TTL_MS);
    assert_eq!(w.read(SEED), w.full(SEED));
    assert!(w.log()[10].1.is_some(), "revalidated, not re-seeded");
    assert_eq!(w.outcomes()[2], "incremental");
}

#[test]
fn a_waiter_wider_than_the_leaders_page_reads_the_exact_url() {
    let mut server = Server::default();
    for i in 0..40u64 {
        server.runs.push(run(
            200 + i,
            &format_ts(ts("2026-10-02T09:00:00Z") + i as i64 * 60),
            "completed",
            "main",
        ));
    }
    let w = world(server);
    let narrow = "repos/o/r/actions/runs?per_page=5";
    let wide = "repos/o/r/actions/runs?per_page=50";
    let (entered, in_upstream) = std::sync::mpsc::channel();
    w.server().hook = Some(Box::new(move |_: &mut Server, endpoint: &str| {
        if endpoint.ends_with("per_page=10") {
            let _ = entered.send(());
            std::thread::sleep(std::time::Duration::from_millis(400));
        }
    }));
    std::thread::scope(|scope| {
        let leader = scope.spawn(|| w.read(narrow));
        in_upstream.recv().unwrap();
        // Joins the narrow leader's flight; its 10 stored runs cannot
        // answer `per_page=50`.
        let waiter = scope.spawn(|| w.read(wide));
        assert_eq!(leader.join().unwrap(), w.full(narrow));
        assert_eq!(waiter.join().unwrap(), w.full(wide));
    });
    assert!(w.requests().contains(&wide.to_string()));
}

#[test]
fn a_query_the_merge_cannot_reproduce_reads_the_exact_url() {
    let w = world(many_comments(40));
    let endpoint = "repos/o/r/issues/5/comments?page=2";
    assert_eq!(w.read(endpoint), w.full(endpoint));
    assert_eq!(w.requests(), [endpoint]);
    assert_eq!(w.ledger(), [entry("full", 1, None)]);
}

fn run(id: u64, created: &str, status: &'static str, branch: &'static str) -> Run {
    Run {
        id,
        created: ts(created),
        updated: ts(created),
        status,
        branch,
        attempt: 1,
    }
}

const RUNS: &str = "repos/o/r/actions/runs?per_page=3&branch=main";
const RUNS_SEED: &str = "repos/o/r/actions/runs?branch=main&per_page=10";

fn four_runs() -> Server {
    Server {
        runs: vec![
            run(100, "2026-10-02T09:00:00Z", "completed", "main"),
            run(101, "2026-10-02T10:00:00Z", "in_progress", "main"),
            run(102, "2026-10-02T10:30:00Z", "completed", "main"),
            run(103, "2026-10-02T10:40:00Z", "completed", "dev"),
        ],
        ..Server::default()
    }
}

#[test]
fn a_run_list_seeds_only_the_width_its_callers_need_and_merges_changes() {
    let w = world(four_runs());
    assert_eq!(w.read(RUNS), w.full(RUNS));
    {
        let mut server = w.server();
        server.runs[1].status = "completed";
        server.runs[1].updated = ts("2026-10-02T11:00:00Z");
        server
            .runs
            .push(run(104, "2026-10-02T11:10:00Z", "queued", "main"));
        server
            .runs
            .push(run(105, "2026-10-02T11:10:00Z", "queued", "main"));
    }
    w.advance(RUNS_TTL_MS);
    let merged = w.read(RUNS);
    assert_eq!(merged, w.full(RUNS));
    assert!(String::from_utf8_lossy(&merged).starts_with("{\"total_count\":5,"));
    // A caller's `per_page=3` needs the newest 10 runs at most, not 100.
    assert_eq!(w.requests(), [RUNS_SEED, RUNS_SEED]);
    assert_eq!(
        w.ledger(),
        [entry("full", 1, None), entry("incremental", 1, Some(3))]
    );
}

#[test]
fn a_rerun_of_a_finished_run_is_seen_in_the_run_list_on_the_next_refresh() {
    let w = world(four_runs());
    w.read(RUNS);
    {
        // `gh run rerun 100` from another machine: same id and created_at,
        // a new attempt, queued again.
        let mut server = w.server();
        server.runs[0].status = "queued";
        server.runs[0].attempt = 2;
        server.runs[0].updated = ts("2026-10-02T11:59:00Z");
    }
    w.advance(RUNS_TTL_MS);
    let merged = w.read(RUNS);
    assert_eq!(merged, w.full(RUNS));
    let wide = "repos/o/r/actions/runs?branch=main";
    assert_eq!(w.read(wide), w.full(wide));
    assert!(String::from_utf8_lossy(&w.read(wide)).contains(r#""run_attempt":2"#));
}

#[test]
fn a_wider_caller_reseeds_a_narrow_run_list() {
    let mut server = Server::default();
    for i in 0..40u64 {
        server.runs.push(run(
            200 + i,
            &format_ts(ts("2026-10-02T09:00:00Z") + i as i64 * 60),
            "completed",
            "main",
        ));
    }
    let w = world(server);
    let narrow = "repos/o/r/actions/runs?per_page=5";
    let wide = "repos/o/r/actions/runs?per_page=40";
    assert_eq!(w.read(narrow), w.full(narrow));
    assert_eq!(w.read(wide), w.full(wide));
    assert_eq!(w.read(narrow), w.full(narrow));
    assert_eq!(
        w.requests(),
        [
            "repos/o/r/actions/runs?per_page=10",
            "repos/o/r/actions/runs?per_page=100",
        ]
    );
}

#[test]
fn a_body_the_merge_cannot_rebuild_falls_back_to_the_exact_url() {
    let pretty = b"[\n  {\"id\": 1}\n]".to_vec();
    let mut server = Server::default();
    server.raw.insert(SEED.into(), pretty.clone());
    server.raw.insert(COMMENTS.into(), pretty.clone());
    let w = world(server);
    assert_eq!(w.read(COMMENTS), pretty);
    assert_eq!(w.requests(), [SEED, COMMENTS]);
    // The collection is marked unmergeable: later reads go straight to the
    // phase-1 path, which is within its TTL here.
    assert_eq!(w.read(COMMENTS), pretty);
    assert_eq!(w.requests().len(), 2);
    assert_eq!(
        w.ledger(),
        [entry("full", 2, None), entry("cache", 0, None)]
    );
    // After the retry interval the merge is tried again.
    w.server().raw.clear();
    w.server().comments = three_comments().comments;
    w.advance(UNMERGEABLE_RETRY_MS);
    assert_eq!(w.read(COMMENTS), w.full(COMMENTS));
    assert_eq!(w.requests()[2], SEED);
}

const JOBS: &str = "repos/o/r/actions/runs/7/jobs";

fn done(at: &str) -> Step {
    Step {
        status: "completed",
        completed_at: Some(ts(at)),
    }
}

#[test]
fn a_rerun_outside_clud_is_seen_in_a_finished_jobs_listing_after_its_ttl() {
    let w = world(Server {
        jobs: vec![done("2026-10-02T11:00:00Z"), done("2026-10-02T11:05:00Z")],
        ..Server::default()
    });
    assert_eq!(w.read(JOBS), w.full(JOBS));
    // No write names the run: still revalidated once its TTL passes, for
    // free while nothing changed.
    w.advance(RUNS_TTL_MS);
    assert_eq!(w.read(JOBS), w.full(JOBS));
    w.server().jobs[1] = Step {
        status: "queued",
        completed_at: None,
    };
    w.advance(RUNS_TTL_MS);
    assert_eq!(w.read(JOBS), w.full(JOBS));
    assert_eq!(w.requests(), [JOBS, JOBS, JOBS]);
    assert_eq!(w.outcomes(), ["full", "304", "full"]);
}

#[test]
fn a_check_run_added_after_the_rest_finished_is_seen_after_the_ttl() {
    let checks = "repos/o/r/commits/0123456789abcdef0123456789abcdef01234567/check-runs";
    let w = world(Server {
        checks: vec![done("2026-10-02T11:00:00Z")],
        ..Server::default()
    });
    w.read(checks);
    w.advance(100 * DEFAULT_TTL_MS);
    w.read(checks);
    w.server().checks.push(Step {
        status: "queued",
        completed_at: None,
    });
    w.advance(DEFAULT_TTL_MS);
    assert_eq!(w.read(checks), w.full(checks));
    assert_eq!(w.outcomes(), ["full", "304", "full"]);
}

#[test]
fn the_invalidate_route_takes_valid_tags_or_falls_back_to_global() {
    let w = world(Server::default());
    let store = w.broker.store().unwrap();
    let run7 = ["run:7".to_string()];
    let other = ["run:8".to_string()];
    w.advance(1);
    assert_eq!(w.broker.handle_invalidate(br#"{"tags":["run:7"]}"#).0, 200);
    let stamp = store.stale_after(&run7).unwrap();
    assert!(stamp > 0);
    assert_eq!(store.stale_after(&other).unwrap(), 0);
    for body in [
        &b"{}"[..],
        br#"{"tags":["bad tag"]}"#,
        br#"{"tags":[]}"#,
        b"junk",
    ] {
        w.advance(1);
        assert_eq!(w.broker.handle_invalidate(body).0, 200);
        assert!(store.stale_after(&other).unwrap() > stamp, "{body:?}");
    }
}

#[test]
fn the_ledger_route_returns_rows_with_removed_counts() {
    let w = world(three_comments());
    w.read(COMMENTS);
    w.server().comments.remove(0);
    w.advance(DEFAULT_TTL_MS);
    w.read(COMMENTS);
    let (status, body) = w.broker.handle_ledger();
    assert_eq!(status, 200);
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["outcome"], "incremental");
    assert_eq!(rows[1]["removed"], 1);
    assert!(rows[0].get("removed").is_none());
}

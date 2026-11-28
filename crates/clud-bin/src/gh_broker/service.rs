//! The daemon half of the read broker (#1743): TTL, single-flight,
//! conditional revalidation, write invalidation and the ledger, plus merged
//! collection reads ([`merged`]) and targeted invalidation
//! ([`super::scope`]).
//!
//! One [`GhBroker`] lives in the daemon's HTTP thread. Each `/gh/read`
//! request runs on its own thread so a slow upstream never blocks the
//! dashboard, and concurrent reads of one key share one upstream request.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use kernal_api::hash::Sha256Hasher as Sha256;

use super::budget::{Budget, Window};
use super::collection;
use super::scope;
use super::store::{LedgerEntry, ObjectMeta, Store};
use super::upstream::{Response, Upstream, UpstreamRequest};
use super::{ReadReply, ReadRequest};

mod merged;
mod watch;

pub use watch::{WatchReply, WatchRequest, MAX_WATCH_ENDPOINTS, MAX_WATCH_MS};

/// Workflow runs and jobs change quickly while a run is live.
pub const RUNS_TTL_MS: u64 = 30_000;
pub const DEFAULT_TTL_MS: u64 = 60_000;
/// Bodies above this are served but not cached.
const MAX_CACHED_BODY: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr)]
pub enum Outcome {
    /// Served from the store within its TTL, or by joining another
    /// request's upstream fetch: no upstream request of its own.
    #[strum(serialize = "cache")]
    Cache,
    /// Revalidated with `If-None-Match` / `If-Modified-Since`; `304`.
    #[strum(serialize = "304")]
    NotModified,
    /// A merged collection where at least one page was re-sent (`200`);
    /// the ledger records the objects it added or changed and removed.
    #[strum(serialize = "incremental")]
    Incremental,
    #[strum(serialize = "full")]
    Full,
    /// Upstream answered non-2xx; the shim reruns the call on the real
    /// `gh` so its error output is `gh`'s own.
    #[strum(serialize = "passthrough")]
    Passthrough,
    /// Below the rate-limit floor and not interactive: no upstream
    /// request; served a stored copy marked stale, or sent to the real `gh`.
    #[strum(serialize = "deferred")]
    Deferred,
    #[strum(serialize = "error")]
    Error,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    Passthrough(u16),
    /// Below the rate-limit floor with nothing stored to serve: the shim
    /// runs the real `gh`.
    Deferred,
    Failed(String),
}

impl ReadError {
    fn outcome(&self) -> Outcome {
        match self {
            ReadError::Passthrough(_) => Outcome::Passthrough,
            ReadError::Deferred => Outcome::Deferred,
            ReadError::Failed(_) => Outcome::Error,
        }
    }
}

/// One answered read and what it cost, for the ledger.
struct Served {
    result: Result<Response, ReadError>,
    outcome: Outcome,
    upstream_requests: u32,
    rate: Option<u64>,
    /// Merged reads: objects added or changed by the upstream fetch.
    changed: Option<u32>,
    /// Merged reads: objects that dropped out (deleted upstream).
    removed: Option<u32>,
    /// Served from the store below the rate-limit floor.
    stale: Option<Stale>,
}

/// A stored copy served instead of a refresh below the rate-limit floor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Stale {
    /// When the copy was fetched (Unix ms).
    pub fetched_at_ms: u64,
    /// The window that deferred the refresh.
    pub remaining: u64,
    pub limit: u64,
    pub reset_s: u64,
}

impl Served {
    fn new(
        result: Result<Response, ReadError>,
        outcome: Outcome,
        upstream_requests: u32,
        rate: Option<u64>,
    ) -> Self {
        Self {
            result,
            outcome,
            upstream_requests,
            rate,
            changed: None,
            removed: None,
            stale: None,
        }
    }

    fn cache(response: Response) -> Self {
        Self::new(Ok(response), Outcome::Cache, 0, None)
    }

    fn failed(error: ReadError, upstream_requests: u32, rate: Option<u64>) -> Self {
        let outcome = error.outcome();
        Self::new(Err(error), outcome, upstream_requests, rate)
    }

    /// A stored copy served below the rate-limit floor.
    fn deferred(response: Response, fetched_at_ms: u64, window: Window) -> Self {
        let mut served = Self::new(Ok(response), Outcome::Deferred, 0, None);
        served.stale = Some(Stale {
            fetched_at_ms,
            remaining: window.remaining,
            limit: window.limit,
            reset_s: window.reset_s,
        });
        served
    }
}

/// One validated read, as the service sees it.
pub struct BrokerRead<'a> {
    pub gh: &'a Path,
    pub endpoint: &'a str,
    pub hostname: Option<&'a str>,
    pub env: &'a [(String, String)],
    pub session_id: Option<&'a str>,
    pub fresh: bool,
    /// The calling shim had a terminal on stdin: a person is waiting, so the
    /// rate-limit floor never defers this read.
    pub interactive: bool,
    /// Leave cache hits out of the ledger (a subscription re-reads its keys
    /// every few seconds; only reads that reach upstream are worth a row).
    pub quiet: bool,
}

impl BrokerRead<'_> {
    /// The caller's identity alone (forwarded env and `gh` path), hashed:
    /// the rate-limit window it spends from.
    fn identity(&self) -> String {
        self.key_for("")
    }

    fn label(&self) -> String {
        format!(
            "{}/{}",
            self.hostname.unwrap_or(""),
            self.endpoint.trim_start_matches('/')
        )
    }

    /// The store key: endpoint and host, plus a hash of the caller's
    /// identity (forwarded env and `gh` path), so two tokens never share a
    /// cached body. Secrets are hashed, never stored.
    fn key(&self) -> String {
        self.key_for(&self.label())
    }

    /// The store key of `label` under this read's identity.
    fn key_for(&self, label: &str) -> String {
        let mut identity: Vec<&(String, String)> = self.env.iter().collect();
        identity.sort();
        let mut hasher = Sha256::new();
        for (k, v) in identity {
            hasher.update(k.as_bytes());
            hasher.update([0]);
            hasher.update(v.as_bytes());
            hasher.update([0]);
        }
        hasher.update(self.gh.to_string_lossy().as_bytes());
        hasher.update([0]);
        hasher.update(label.as_bytes());
        let digest = hasher.finalize();
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }
}

/// TTL for an endpoint: runs and jobs 30 s, everything else 60 s.
pub fn ttl_ms(endpoint: &str) -> u64 {
    let path = endpoint.split('?').next().unwrap_or(endpoint);
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let live = segments
        .windows(2)
        .any(|w| w[0] == "actions" && matches!(w[1], "runs" | "jobs"));
    if live {
        RUNS_TTL_MS
    } else {
        DEFAULT_TTL_MS
    }
}

#[derive(Default)]
struct Flight {
    result: Mutex<Option<Result<Response, ReadError>>>,
    ready: Condvar,
}

impl Flight {
    fn wait(&self) -> Result<Response, ReadError> {
        let mut slot = self.result.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if let Some(result) = slot.as_ref() {
                return result.clone();
            }
            slot = self.ready.wait(slot).unwrap_or_else(|p| p.into_inner());
        }
    }

    fn finish(&self, result: Result<Response, ReadError>) {
        let mut slot = self.result.lock().unwrap_or_else(|p| p.into_inner());
        if slot.is_none() {
            *slot = Some(result);
        }
        self.ready.notify_all();
    }
}

pub type Clock = Box<dyn Fn() -> u64 + Send + Sync>;

pub struct GhBroker {
    store_path: PathBuf,
    store: Mutex<Option<Arc<Store>>>,
    upstream: Box<dyn Upstream>,
    clock: Clock,
    flights: Mutex<HashMap<String, Arc<Flight>>>,
    budget: Budget,
    reserve_pct: ReservePct,
    /// How a blocked subscription waits (ms); tests advance a fake clock.
    sleep: Sleep,
}

pub type Sleep = Box<dyn Fn(u64) + Send + Sync>;

/// The rate-limit reserve in percent, read when a refresh is due.
pub type ReservePct = Box<dyn Fn() -> u64 + Send + Sync>;

/// The daemon's reserve: `CLUD_GH_BROKER_RESERVE_PCT`, else the setting
/// `git.gh_read_broker_reserve_pct`, else 10%. The setting is re-read at
/// most once a minute.
fn reserve_from_settings() -> ReservePct {
    let cache: Mutex<Option<(u64, u64)>> = Mutex::new(None);
    Box::new(move || {
        let now = unix_ms();
        let mut slot = cache.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((at, pct)) = *slot {
            if now.saturating_sub(at) < 60_000 {
                return pct;
            }
        }
        let env = std::env::var("CLUD_GH_BROKER_RESERVE_PCT").ok();
        let setting = crate::clud_settings::load_gh_read_broker_reserve_pct()
            .ok()
            .flatten();
        let pct = super::budget::reserve_pct_from(env.as_deref(), setting);
        *slot = Some((now, pct));
        pct
    })
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl GhBroker {
    /// The daemon's broker: store opened lazily on the first read, real
    /// `gh` upstream, wall clock.
    pub fn for_state_dir(state_dir: &Path) -> Self {
        Self::new(
            state_dir.join(super::STORE_FILE),
            Box::new(super::upstream::GhCli),
            Box::new(unix_ms),
        )
        .with_reserve(reserve_from_settings())
    }

    pub fn new(store_path: PathBuf, upstream: Box<dyn Upstream>, clock: Clock) -> Self {
        Self {
            store_path,
            store: Mutex::new(None),
            upstream,
            clock,
            flights: Mutex::new(HashMap::new()),
            budget: Budget::default(),
            reserve_pct: Box::new(|| super::budget::DEFAULT_RESERVE_PCT),
            sleep: Box::new(|ms| std::thread::sleep(std::time::Duration::from_millis(ms))),
        }
    }

    /// Replace how a blocked subscription waits.
    pub fn with_sleep(mut self, sleep: Sleep) -> Self {
        self.sleep = sleep;
        self
    }

    /// Replace how the rate-limit reserve is read.
    pub fn with_reserve(mut self, reserve_pct: ReservePct) -> Self {
        self.reserve_pct = reserve_pct;
        self
    }

    /// One upstream request for `read`'s identity; its rate-limit headers
    /// feed the floor.
    fn fetch_upstream(
        &self,
        read: &BrokerRead<'_>,
        request: &UpstreamRequest<'_>,
    ) -> Result<Response, String> {
        let response = self.upstream.fetch(request)?;
        self.budget.observe(&read.identity(), &response);
        Ok(response)
    }

    /// The window that defers `read`'s refresh, if it is below the floor
    /// and nobody is waiting at a terminal.
    fn deferral(&self, read: &BrokerRead<'_>) -> Option<Window> {
        if read.interactive {
            return None;
        }
        self.budget
            .deferred(&read.identity(), (self.reserve_pct)(), (self.clock)())
    }

    fn store(&self) -> Result<Arc<Store>, ReadError> {
        let mut slot = self.store.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(store) = slot.as_ref() {
            return Ok(Arc::clone(store));
        }
        let store = Arc::new(Store::open(&self.store_path).map_err(ReadError::Failed)?);
        *slot = Some(Arc::clone(&store));
        Ok(store)
    }

    pub fn ledger(&self) -> Result<Vec<LedgerEntry>, String> {
        self.store()
            .map_err(|e| format!("{e:?}"))
            .and_then(|s| s.ledger())
    }

    /// Mark every cached read stale. In-flight
    /// fetches are detached, so a read issued after the write never joins
    /// a fetch that began before it.
    pub fn invalidate(&self) -> Result<(), String> {
        let store = self.store().map_err(|e| format!("{e:?}"))?;
        store.invalidate((self.clock)())?;
        self.detach_flights();
        Ok(())
    }

    /// Mark stale only the reads that carry one of `tags`
    /// ([`scope::key_tags`]).
    pub fn invalidate_tags(&self, tags: &[String]) -> Result<(), String> {
        let store = self.store().map_err(|e| format!("{e:?}"))?;
        store.invalidate_tags(tags, (self.clock)())?;
        self.detach_flights();
        Ok(())
    }

    fn detach_flights(&self) {
        self.flights
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
    }

    /// The cached body for `key` if no write since its fetch could have
    /// changed it, and it is within its TTL (unless `fresh`).
    fn fresh_hit(&self, store: &Store, key: &str, endpoint: &str, fresh: bool) -> Option<Response> {
        let (meta, body) = store.get(key).ok()??;
        let stale_after = store
            .stale_after(&scope::key_tags(endpoint))
            .unwrap_or(u64::MAX);
        if meta.fetched_at_ms <= stale_after {
            return None;
        }
        let now = (self.clock)();
        let within_ttl = !fresh && now.saturating_sub(meta.fetched_at_ms) < ttl_ms(endpoint);
        within_ttl.then(|| Response {
            status: meta.status,
            headers: meta.headers,
            body: Arc::new(body),
        })
    }

    pub fn read(&self, read: &BrokerRead<'_>) -> Result<Response, ReadError> {
        self.read_marked(read).0
    }

    /// [`Self::read`], plus the stale mark of a copy served below the
    /// rate-limit floor.
    pub fn read_marked(
        &self,
        read: &BrokerRead<'_>,
    ) -> (Result<Response, ReadError>, Option<Stale>) {
        let served = self.read_inner(read);
        let skip = read.quiet && served.outcome == Outcome::Cache;
        if let (false, Ok(store)) = (skip, self.store()) {
            let _ = store.append_ledger(&LedgerEntry {
                ts_ms: (self.clock)(),
                session_id: read.session_id.map(str::to_string),
                key: read.label(),
                outcome: served.outcome.as_str().to_string(),
                upstream_requests: served.upstream_requests,
                rate_remaining: served.rate,
                changed: served.changed,
                removed: served.removed,
            });
        }
        (served.result, served.stale)
    }

    fn read_inner(&self, read: &BrokerRead<'_>) -> Served {
        let store = match self.store() {
            Ok(store) => store,
            Err(error) => return Served::failed(error, 0, None),
        };
        let mut spent = 0;
        if let Some(plan) = collection::plan(read.endpoint) {
            match self.read_collection(&store, read, &plan) {
                merged::Merged::Served(served) => return served,
                merged::Merged::Fallback(requests) => spent = requests,
            }
        }
        let mut served = self.read_object(&store, read);
        served.upstream_requests += spent;
        served
    }

    /// Join the in-flight fetch of `key`, or become its leader (`true`).
    fn join_flight(&self, key: &str) -> (Arc<Flight>, bool) {
        let mut flights = self.flights.lock().unwrap_or_else(|p| p.into_inner());
        match flights.get(key) {
            Some(flight) => (Arc::clone(flight), false),
            None => {
                let flight = Arc::new(Flight::default());
                flights.insert(key.to_string(), Arc::clone(&flight));
                (flight, true)
            }
        }
    }

    /// The phase-1 path: one object keyed by its exact URL.
    fn read_object(&self, store: &Store, read: &BrokerRead<'_>) -> Served {
        let key = read.key();
        if let Some(hit) = self.fresh_hit(store, &key, read.endpoint, read.fresh) {
            return Served::cache(hit);
        }
        if let Some(window) = self.deferral(read) {
            // Below the floor: the stored copy, marked stale, or the real
            // `gh`. Never a fetch, never an invented body.
            return match store.get(&key).ok().flatten() {
                Some((meta, body)) => Served::deferred(
                    Response {
                        status: meta.status,
                        headers: meta.headers,
                        body: Arc::new(body),
                    },
                    meta.fetched_at_ms,
                    window,
                ),
                None => Served::failed(ReadError::Deferred, 0, None),
            };
        }
        let (flight, leader) = self.join_flight(&key);
        if !leader {
            return match flight.wait() {
                Ok(response) => Served::cache(response),
                Err(error) => Served::failed(error, 0, None),
            };
        }
        let landing = Landing {
            broker: self,
            key: &key,
            flight: &flight,
        };
        let served = self.fetch(store, &key, read);
        landing.finish(served.result.clone());
        served
    }

    /// The leader's work: recheck the store, then one conditional request.
    fn fetch(&self, store: &Store, key: &str, read: &BrokerRead<'_>) -> Served {
        // Another leader may have landed between our miss and our flight.
        if let Some(hit) = self.fresh_hit(store, key, read.endpoint, read.fresh) {
            return Served::cache(hit);
        }
        let cached = store.get(key).ok().flatten();
        let started = (self.clock)();
        let request = UpstreamRequest {
            gh: read.gh,
            endpoint: read.endpoint,
            hostname: read.hostname,
            env: read.env,
            if_none_match: cached.as_ref().and_then(|(m, _)| m.etag.as_deref()),
            if_modified_since: cached
                .as_ref()
                .filter(|(m, _)| m.etag.is_none())
                .and_then(|(m, _)| m.last_modified.as_deref()),
        };
        let response = match self.fetch_upstream(read, &request) {
            Ok(response) => response,
            Err(error) => return Served::failed(ReadError::Failed(error), 1, None),
        };
        let rate = response
            .header("x-ratelimit-remaining")
            .and_then(|v| v.parse().ok());
        if response.status == 304 {
            let Some((mut meta, body)) = cached else {
                return Served::failed(ReadError::Passthrough(304), 1, rate);
            };
            meta.fetched_at_ms = started;
            if let Some(etag) = response.header("etag") {
                meta.etag = Some(etag.to_string());
            }
            let _ = store.put(key, &meta, None);
            let served = Response {
                status: meta.status,
                headers: meta.headers,
                body: Arc::new(body),
            };
            return Served::new(Ok(served), Outcome::NotModified, 1, rate);
        }
        if !(200..300).contains(&response.status) {
            return Served::failed(ReadError::Passthrough(response.status), 1, rate);
        }
        if response.status == 200 && response.body.len() <= MAX_CACHED_BODY {
            let meta = ObjectMeta {
                label: read.label(),
                status: response.status,
                headers: response.headers.clone(),
                etag: response.header("etag").map(str::to_string),
                last_modified: response.header("last-modified").map(str::to_string),
                fetched_at_ms: started,
            };
            let _ = store.put(key, &meta, Some(&response.body));
        }
        Served::new(Ok(response), Outcome::Full, 1, rate)
    }

    /// The daemon's `/gh/read` handler body: JSON in, `(status, JSON)` out.
    /// 200 carries a [`ReadReply`]; 409 means "run the real `gh`"; 400/502
    /// are errors the shim also answers by running the real `gh`.
    pub fn handle_http(&self, body: &[u8], self_exe: &Path, home: Option<&Path>) -> (u16, Vec<u8>) {
        let request: ReadRequest = match serde_json::from_slice(body) {
            Ok(request) => request,
            Err(error) => return (400, error_json(&format!("invalid request: {error}"))),
        };
        let gh = PathBuf::from(&request.gh);
        let endpoints = [request.endpoint.as_str()];
        let caller = Caller {
            gh: &gh,
            hostname: request.hostname.as_deref(),
            env: &request.env,
        };
        if let Err(message) = caller.validate(&endpoints, self_exe, home) {
            return (400, error_json(message));
        }
        let read = BrokerRead {
            gh: &gh,
            endpoint: &request.endpoint,
            hostname: request.hostname.as_deref(),
            env: &request.env,
            session_id: request.session_id.as_deref(),
            fresh: request.fresh,
            interactive: request.interactive,
            quiet: false,
        };
        match self.read_marked(&read) {
            (Ok(response), stale) => {
                let reply = ReadReply {
                    status: response.status,
                    headers: response.headers,
                    body_b64: base64::engine::general_purpose::STANDARD.encode(&*response.body),
                    outcome: "ok".to_string(),
                    stale: stale.map(|stale| super::StaleNote {
                        age_ms: (self.clock)().saturating_sub(stale.fetched_at_ms),
                        remaining: stale.remaining,
                        limit: stale.limit,
                        reset_s: stale.reset_s,
                    }),
                };
                match serde_json::to_vec(&reply) {
                    Ok(bytes) => (200, bytes),
                    Err(error) => (502, error_json(&error.to_string())),
                }
            }
            (Err(ReadError::Passthrough(status)), _) => {
                (409, error_json(&format!("upstream status {status}")))
            }
            (Err(ReadError::Deferred), _) => (
                409,
                error_json("deferred below the rate-limit floor; nothing stored"),
            ),
            (Err(ReadError::Failed(error)), _) => (502, error_json(&error)),
        }
    }
}

/// Who a route request reads as: the `gh` it runs and the identity it
/// forwards.
struct Caller<'a> {
    gh: &'a Path,
    hostname: Option<&'a str>,
    env: &'a [(String, String)],
}

impl Caller<'_> {
    /// The checks every route applies before running anything: the target
    /// is a real `gh`, the host and env keys are the forwarded ones, and
    /// each endpoint is a brokerable REST read. The shim already classified
    /// the read; the daemon re-checks so a route can only ever run
    /// `gh api -i <REST path>`.
    fn validate(
        &self,
        endpoints: &[&str],
        self_exe: &Path,
        home: Option<&Path>,
    ) -> Result<(), &'static str> {
        let dirs = crate::shim_registry::shim_dirs(self_exe, None, home);
        let named_gh = crate::shim_registry::invoked_name(self.gh.as_os_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("gh"));
        if !named_gh || !crate::shim_registry::valid_target(self.gh, self_exe, &dirs) {
            return Err("gh target is not a real gh executable");
        }
        let hostname_ok = self
            .hostname
            .is_none_or(|h| super::classify::valid_hostname(h).is_some());
        let env_ok = self
            .env
            .iter()
            .all(|(key, _)| super::FORWARDED_ENV.contains(&key.as_str()));
        let endpoints_ok = endpoints
            .iter()
            .all(|endpoint| super::classify::brokerable_endpoint(endpoint));
        if !endpoints_ok || !hostname_ok || !env_ok {
            return Err("not a brokerable gh api read");
        }
        Ok(())
    }
}

/// Shim -> daemon `/gh/invalidate` body. No (or invalid) `tags` means
/// every read, the phase-1 behavior an older shim still asks for.
#[derive(serde::Deserialize)]
struct InvalidateRequest {
    #[serde(default)]
    tags: Option<Vec<String>>,
}

impl GhBroker {
    /// The daemon's `/gh/invalidate` handler body.
    pub fn handle_invalidate(&self, body: &[u8]) -> (u16, Vec<u8>) {
        let tags = serde_json::from_slice::<InvalidateRequest>(body)
            .ok()
            .and_then(|request| request.tags)
            .filter(|tags| {
                !tags.is_empty()
                    && tags.len() <= scope::MAX_TAGS
                    && tags.iter().all(|tag| scope::valid_tag(tag))
            });
        let result = match tags {
            Some(tags) => self.invalidate_tags(&tags),
            None => self.invalidate(),
        };
        match result {
            Ok(()) => (200, b"{}".to_vec()),
            Err(error) => (500, error_json(&error)),
        }
    }
}

impl GhBroker {
    /// The daemon's `/gh/ledger` handler body: the newest
    /// [`super::LEDGER_ROWS`] rows, oldest first.
    pub fn handle_ledger(&self) -> (u16, Vec<u8>) {
        let rows = self
            .store()
            .map_err(|e| format!("{e:?}"))
            .and_then(|store| store.ledger_tail(super::LEDGER_ROWS));
        match rows.and_then(|rows| serde_json::to_vec(&rows).map_err(|e| e.to_string())) {
            Ok(bytes) => (200, bytes),
            Err(error) => (500, error_json(&error)),
        }
    }
}

fn error_json(message: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "error": message })).unwrap_or_default()
}

/// Lands the leader's flight exactly once, even if the fetch panics: waiters
/// are released and the key is free for the next read.
struct Landing<'a> {
    broker: &'a GhBroker,
    key: &'a str,
    flight: &'a Arc<Flight>,
}

impl Landing<'_> {
    fn finish(self, result: Result<Response, ReadError>) {
        self.flight.finish(result);
    }
}

impl Drop for Landing<'_> {
    fn drop(&mut self) {
        self.flight
            .finish(Err(ReadError::Failed("broker fetch aborted".into())));
        let mut flights = self
            .broker
            .flights
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        if flights
            .get(self.key)
            .is_some_and(|current| Arc::ptr_eq(current, self.flight))
        {
            flights.remove(self.key);
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod phase2_tests;

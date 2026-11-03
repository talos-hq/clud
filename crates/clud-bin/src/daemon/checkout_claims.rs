//! Live checkout presence and exclusive mutation claims (#1342).
//!
//! A presence belongs to a daemon connection. Removing that connection drops
//! its claim in the same mutex operation, including after an abrupt exit.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::io_helpers::read_json_file;
use super::paths::daemon_info_path;
use super::types::DaemonInfo;

const CHECKOUT_RPC_TIMEOUT: Duration = Duration::from_secs(2);
const PRESENCE_POLL_INTERVAL: Duration = Duration::from_millis(500);

fn claim_intent_path(state_dir: &Path, session_id: &str) -> PathBuf {
    let encoded = session_id
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    state_dir.join("checkout-claims").join(encoded)
}

fn set_claim_intent(path: &Path, claimed: bool) -> io::Result<()> {
    if claimed {
        fs::create_dir_all(path.parent().expect("claim intent has parent"))?;
        fs::write(path, b"claimed")
    } else if path.exists() {
        fs::remove_file(path)
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(super) struct CheckoutKey {
    pub(super) worktree: PathBuf,
    pub(super) common_git_dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CheckoutOccupant {
    pub(crate) session_id: String,
    pub(super) pid: u32,
    pub(crate) tool: String,
    pub(crate) run: Option<String>,
    pub(super) started_at: u64,
    pub(super) claimed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ClaimRefusal {
    UnknownSession,
    StartupGrace,
    HeldBy(CheckoutOccupant),
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "checkout_op", rename_all = "snake_case")]
pub(super) enum CheckoutRequest {
    Present {
        checkout: CheckoutKey,
        occupant: CheckoutOccupant,
    },
    Who {
        checkout: CheckoutKey,
        exclude_session: Option<String>,
    },
    WhoAll,
    Claim {
        checkout: CheckoutKey,
        session_id: String,
        restoring: bool,
    },
    Release {
        checkout: CheckoutKey,
        session_id: String,
        force: bool,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "checkout_op", rename_all = "snake_case")]
pub(super) enum CheckoutReply {
    Present {
        others: Vec<CheckoutOccupant>,
    },
    Who {
        occupants: Vec<CheckoutOccupant>,
    },
    WhoAll {
        occupants: Vec<(CheckoutKey, CheckoutOccupant)>,
    },
    Claimed,
    Released {
        released: bool,
    },
    Refused {
        reason: String,
        holder: Option<CheckoutOccupant>,
    },
}

#[derive(Clone)]
pub(super) struct CheckoutClaimRegistry {
    inner: Arc<Mutex<ClaimState>>,
    grace_until: Instant,
}

#[derive(Default)]
struct ClaimState {
    connections: HashMap<u64, Presence>,
}

struct Presence {
    checkout: CheckoutKey,
    occupant: CheckoutOccupant,
}

impl CheckoutClaimRegistry {
    pub(super) fn new(restart_grace: Duration) -> Self {
        Self {
            inner: Arc::new(Mutex::new(ClaimState::default())),
            grace_until: Instant::now() + restart_grace,
        }
    }

    pub(super) fn register(
        &self,
        connection_id: u64,
        checkout: CheckoutKey,
        mut occupant: CheckoutOccupant,
    ) -> Vec<CheckoutOccupant> {
        let mut state = self.inner.lock().expect("checkout claims poisoned");
        occupant.claimed = state.connections.values().any(|presence| {
            presence.occupant.session_id == occupant.session_id
                && presence.checkout == checkout
                && presence.occupant.claimed
        });
        let others = occupants_in(&state, &checkout, Some(&occupant.session_id));
        state
            .connections
            .retain(|_, presence| presence.occupant.session_id != occupant.session_id);
        state
            .connections
            .insert(connection_id, Presence { checkout, occupant });
        others
    }

    pub(super) fn respond(&self, connection_id: u64, request: CheckoutRequest) -> CheckoutReply {
        match request {
            CheckoutRequest::Present { checkout, occupant } => CheckoutReply::Present {
                others: self.register(connection_id, checkout, occupant),
            },
            CheckoutRequest::Who {
                checkout,
                exclude_session,
            } => CheckoutReply::Who {
                occupants: self.who(&checkout, exclude_session.as_deref()),
            },
            CheckoutRequest::WhoAll => CheckoutReply::WhoAll {
                occupants: self.all(),
            },
            CheckoutRequest::Claim {
                checkout,
                session_id,
                restoring,
            } => match self.claim(&session_id, &checkout, restoring) {
                Ok(()) => CheckoutReply::Claimed,
                Err(ClaimRefusal::UnknownSession) => CheckoutReply::Refused {
                    reason: "session has no live checkout connection".to_string(),
                    holder: None,
                },
                Err(ClaimRefusal::StartupGrace) => CheckoutReply::Refused {
                    reason: "daemon restart grace period; retry shortly".to_string(),
                    holder: None,
                },
                Err(ClaimRefusal::HeldBy(holder)) => CheckoutReply::Refused {
                    reason: "checkout is already claimed".to_string(),
                    holder: Some(holder),
                },
            },
            CheckoutRequest::Release {
                checkout,
                session_id,
                force,
            } => CheckoutReply::Released {
                released: if force {
                    self.force_release(&session_id, &checkout)
                } else {
                    self.release(&session_id, &checkout)
                },
            },
        }
    }

    pub(super) fn claim(
        &self,
        session_id: &str,
        checkout: &CheckoutKey,
        restoring: bool,
    ) -> Result<(), ClaimRefusal> {
        let mut state = self.inner.lock().expect("checkout claims poisoned");
        let owner_id = state
            .connections
            .iter()
            .find(|(_, presence)| {
                presence.occupant.session_id == session_id && presence.checkout == *checkout
            })
            .map(|(id, _)| *id)
            .ok_or(ClaimRefusal::UnknownSession)?;
        if let Some(holder) = state.connections.values().find(|presence| {
            presence.checkout == *checkout
                && presence.occupant.claimed
                && presence.occupant.session_id != session_id
        }) {
            return Err(ClaimRefusal::HeldBy(holder.occupant.clone()));
        }
        if !restoring && Instant::now() < self.grace_until {
            return Err(ClaimRefusal::StartupGrace);
        }
        if let Some(owner) = state.connections.get_mut(&owner_id) {
            owner.occupant.claimed = true;
        }
        Ok(())
    }

    pub(super) fn release(&self, session_id: &str, checkout: &CheckoutKey) -> bool {
        let mut state = self.inner.lock().expect("checkout claims poisoned");
        let Some(owner) = state.connections.values_mut().find(|presence| {
            presence.occupant.session_id == session_id && presence.checkout == *checkout
        }) else {
            return false;
        };
        owner.occupant.claimed = false;
        true
    }

    pub(super) fn disconnect(&self, connection_id: u64) {
        let mut state = self.inner.lock().expect("checkout claims poisoned");
        state.connections.remove(&connection_id);
    }

    fn force_release(&self, session_id: &str, checkout: &CheckoutKey) -> bool {
        let mut state = self.inner.lock().expect("checkout claims poisoned");
        let mut released = false;
        for presence in state.connections.values_mut() {
            if presence.checkout == *checkout
                && presence.occupant.session_id == session_id
                && presence.occupant.claimed
            {
                presence.occupant.claimed = false;
                released = true;
            }
        }
        released
    }

    pub(super) fn who(
        &self,
        checkout: &CheckoutKey,
        exclude_session: Option<&str>,
    ) -> Vec<CheckoutOccupant> {
        let state = self.inner.lock().expect("checkout claims poisoned");
        occupants_in(&state, checkout, exclude_session)
    }

    fn all(&self) -> Vec<(CheckoutKey, CheckoutOccupant)> {
        let state = self.inner.lock().expect("checkout claims poisoned");
        let mut occupants = state
            .connections
            .values()
            .map(|presence| (presence.checkout.clone(), presence.occupant.clone()))
            .collect::<Vec<_>>();
        occupants.sort_by(|a, b| {
            a.0.worktree
                .cmp(&b.0.worktree)
                .then(a.1.session_id.cmp(&b.1.session_id))
        });
        occupants
    }
}

pub(super) fn all_live(state_dir: &Path) -> io::Result<Vec<(CheckoutKey, CheckoutOccupant)>> {
    let (_, reply) = checkout_rpc(state_dir, &CheckoutRequest::WhoAll)?;
    if let CheckoutReply::WhoAll { occupants } = reply {
        Ok(occupants)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected checkout list reply",
        ))
    }
}

pub struct ForegroundCheckoutPresence {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<Option<String>>>,
    claim_intent_path: PathBuf,
}

impl Drop for ForegroundCheckoutPresence {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // The session has exited and restored the terminal by now, so the
        // presence thread's notice is printed here rather than over the
        // harness's live screen (#2163).
        if let Some(Ok(Some(notice))) = self.thread.take().map(JoinHandle::join) {
            eprintln!("{notice}");
        }
        let _ = fs::remove_file(&self.claim_intent_path);
    }
}

pub(super) fn checkout_from(cwd: &Path) -> io::Result<Option<CheckoutKey>> {
    if !crate::loop_spec::git_root_from(cwd).join(".git").exists() {
        return Ok(None);
    }
    let top = crate::worktrees::run_git(cwd, &["rev-parse", "--show-toplevel"])
        .map_err(io::Error::other)?;
    let common = crate::worktrees::run_git(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .map_err(io::Error::other)?;
    Ok(Some(CheckoutKey {
        worktree: fs::canonicalize(top.trim())?,
        common_git_dir: fs::canonicalize(common.trim())?,
    }))
}

/// This foreground process's session id, as recorded in its checkout claim.
pub fn foreground_session_id() -> String {
    format!(
        "clud-{}-{}",
        std::process::id(),
        crate::process_identity::self_start_time()
    )
}

pub fn start_foreground_presence(
    state_dir: &Path,
    cwd: &Path,
    tool: &str,
    run: Option<String>,
    claim: bool,
    interrupted: Arc<AtomicBool>,
) -> io::Result<Option<ForegroundCheckoutPresence>> {
    let Some(checkout) = checkout_from(cwd)? else {
        return if claim {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "checkout claim requires a Git worktree",
            ))
        } else {
            Ok(None)
        };
    };
    let session_id = foreground_session_id();
    let occupant = CheckoutOccupant {
        session_id: session_id.clone(),
        pid: std::process::id(),
        tool: tool.to_string(),
        run,
        started_at: crate::process_identity::self_start_time(),
        claimed: false,
    };
    let (stream, others) = connect_presence(state_dir, &checkout, &occupant)?;
    for other in others {
        eprintln!(
            "[clud] note: session {} is also active in this checkout ({}{}){}",
            other.session_id,
            other.tool,
            other.run.map_or_else(String::new, |run| format!(" {run}")),
            if other.claimed { " [CLAIMED]" } else { "" }
        );
    }
    if claim {
        claim_until_ready(state_dir, &checkout, &session_id, false)?;
    }
    let claim_intent_path = claim_intent_path(state_dir, &session_id);
    if claim {
        set_claim_intent(&claim_intent_path, true)?;
    }
    // Keep checkout identity separate from the session ID used by trash and
    // other features, which may be set by the harness itself.
    std::env::set_var("CLUD_CHECKOUT_SESSION_ID", &session_id);
    let stop = Arc::new(AtomicBool::new(false));
    let thread_stop = Arc::clone(&stop);
    let state_dir = state_dir.to_path_buf();
    let thread = thread::Builder::new()
        .name("clud-checkout-presence".to_string())
        .spawn(move || {
            let end = maintain_presence(stream, &state_dir, &checkout, &occupant, &thread_stop);
            presence_end_notice(end, &interrupted)
        })?;
    Ok(Some(ForegroundCheckoutPresence {
        stop,
        thread: Some(thread),
        claim_intent_path,
    }))
}

pub(crate) fn claimed_by_other(
    state_dir: &Path,
    cwd: &Path,
    session_id: Option<&str>,
) -> io::Result<Option<CheckoutOccupant>> {
    let Some(checkout) = checkout_from(cwd)? else {
        return Ok(None);
    };
    let (_, reply) = checkout_rpc(
        state_dir,
        &CheckoutRequest::Who {
            checkout,
            exclude_session: session_id.map(str::to_string),
        },
    )?;
    match reply {
        CheckoutReply::Who { occupants } => Ok(occupants.into_iter().find(|person| person.claimed)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected checkout query reply",
        )),
    }
}

pub fn run_claim_command(action: &crate::args::ClaimSubcommand) -> i32 {
    let result = (|| -> io::Result<()> {
        let state_dir = super::default_state_dir()?;
        let cwd = std::env::current_dir()?;
        let path = match action {
            crate::args::ClaimSubcommand::Release { checkout, .. } => checkout.as_path(),
            _ => cwd.as_path(),
        };
        let checkout = checkout_from(path)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no Git checkout found at that path",
            )
        })?;
        match action {
            crate::args::ClaimSubcommand::Who => {
                let (_, reply) = checkout_rpc(
                    &state_dir,
                    &CheckoutRequest::Who {
                        checkout,
                        exclude_session: None,
                    },
                )?;
                if let CheckoutReply::Who { occupants } = reply {
                    for occupant in occupants {
                        println!(
                            "{} pid={} {}{}{}",
                            occupant.session_id,
                            occupant.pid,
                            occupant.tool,
                            occupant
                                .run
                                .map_or_else(String::new, |run| format!(" {run}")),
                            if occupant.claimed { " [CLAIMED]" } else { "" }
                        );
                    }
                }
            }
            crate::args::ClaimSubcommand::Acquire => {
                let session_id = std::env::var("CLUD_CHECKOUT_SESSION_ID").map_err(|_| {
                    io::Error::other("no live clud session; start clud in this checkout first")
                })?;
                claim_until_ready(&state_dir, &checkout, &session_id, false)?;
                set_claim_intent(&claim_intent_path(&state_dir, &session_id), true)?;
                println!("Claimed {}", checkout.worktree.display());
            }
            crate::args::ClaimSubcommand::ReleaseOwn => {
                let session_id = std::env::var("CLUD_CHECKOUT_SESSION_ID")
                    .map_err(|_| io::Error::other("no live clud checkout session"))?;
                let reply = checkout_rpc(
                    &state_dir,
                    &CheckoutRequest::Release {
                        checkout,
                        session_id: session_id.clone(),
                        force: false,
                    },
                );
                set_claim_intent(&claim_intent_path(&state_dir, &session_id), false)?;
                let (_, reply) = reply?;
                if let CheckoutReply::Released { released } = reply {
                    println!(
                        "{}",
                        if released {
                            "Claim released"
                        } else {
                            "No own claim"
                        }
                    );
                }
            }
            crate::args::ClaimSubcommand::Release { yes, .. } => {
                release_claim(&state_dir, checkout, *yes)?;
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        eprintln!("[clud] claim: {error}");
        1
    } else {
        0
    }
}

fn release_claim(state_dir: &Path, checkout: CheckoutKey, yes: bool) -> io::Result<()> {
    let (_, owners_reply) = checkout_rpc(
        state_dir,
        &CheckoutRequest::Who {
            checkout: checkout.clone(),
            exclude_session: None,
        },
    )?;
    let owners = match owners_reply {
        CheckoutReply::Who { occupants } => occupants
            .into_iter()
            .filter(|entry| entry.claimed)
            .collect::<Vec<_>>(),
        _ => return Err(io::Error::other("unexpected checkout query reply")),
    };
    if owners.is_empty() {
        println!("No live claim");
        return Ok(());
    }
    if !yes {
        eprint!(
            "Release claim held by {} for {}? [y/N] ",
            owners
                .iter()
                .map(|entry| entry.session_id.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            checkout.worktree.display()
        );
        io::stderr().flush()?;
        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes" | "YES") {
            return Err(io::Error::other("claim release cancelled"));
        }
    }
    let (_, reply) = checkout_rpc(
        state_dir,
        &CheckoutRequest::Release {
            checkout,
            session_id: owners[0].session_id.clone(),
            force: true,
        },
    )?;
    if let CheckoutReply::Released { released } = reply {
        if released {
            for occupant in owners {
                set_claim_intent(&claim_intent_path(state_dir, &occupant.session_id), false)?;
            }
        }
        println!(
            "{}",
            if released {
                "Claim released"
            } else {
                "No live claim"
            }
        );
    }
    Ok(())
}

/// Why the presence thread stopped.
#[derive(Debug)]
enum PresenceEnd {
    /// The session is shutting down.
    Stopped,
    /// Another session obtained this session's claim while the daemon was away.
    ClaimLost(io::Error),
    /// Reconnecting cannot succeed for the life of this process (#1981).
    Disabled(io::Error),
}

/// The notice to show once the session has exited, interrupting the session
/// when its claim was lost. The presence thread runs while the harness owns
/// the terminal, so it must never print itself (#2163).
fn presence_end_notice(end: PresenceEnd, interrupted: &AtomicBool) -> Option<String> {
    match end {
        PresenceEnd::Stopped => None,
        PresenceEnd::ClaimLost(error) => {
            interrupted.store(true, Ordering::SeqCst);
            Some(format!(
                "[clud] checkout claim lost after daemon restart: {error}; stopped this session"
            ))
        }
        PresenceEnd::Disabled(error) => Some(format!(
            "[clud] checkout presence was disabled during this session: {error}"
        )),
    }
}

fn maintain_presence(
    mut stream: TcpStream,
    state_dir: &Path,
    checkout: &CheckoutKey,
    occupant: &CheckoutOccupant,
    stop: &AtomicBool,
) -> PresenceEnd {
    while !stop.load(Ordering::SeqCst) {
        let _ = stream.set_read_timeout(Some(PRESENCE_POLL_INTERVAL));
        let mut byte = [0u8; 1];
        match stream.read(&mut byte) {
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                continue;
            }
            _ => {}
        }
        let reconnected = reconnect_with_backoff(
            stop,
            || reconnect_presence(state_dir, checkout, occupant),
            thread::sleep,
            presence_jitter,
        );
        match reconnected {
            Ok(PresenceReconnect::Connected(next)) => stream = next,
            Ok(PresenceReconnect::ClaimLost(error)) => return PresenceEnd::ClaimLost(error),
            Err(ReconnectEnd::Permanent(error)) => return PresenceEnd::Disabled(error),
            Err(ReconnectEnd::Stopped) => return PresenceEnd::Stopped,
        }
    }
    PresenceEnd::Stopped
}

/// Longest wait between presence reconnect attempts (#1981).
const PRESENCE_RECONNECT_CAP: Duration = Duration::from_secs(30);

enum PresenceReconnect {
    Connected(TcpStream),
    ClaimLost(io::Error),
}

#[derive(Debug)]
enum ReconnectEnd {
    /// The session is shutting down.
    Stopped,
    /// Retrying cannot succeed for the life of this process.
    Permanent(io::Error),
}

/// One reconnect: make sure the daemon is ours, re-register presence, and
/// restore a claim this session held before the daemon went away.
fn reconnect_presence(
    state_dir: &Path,
    checkout: &CheckoutKey,
    occupant: &CheckoutOccupant,
) -> io::Result<PresenceReconnect> {
    super::client::ensure_daemon(state_dir)?;
    let (next, _) = connect_presence(state_dir, checkout, occupant)?;
    if claim_intent_path(state_dir, &occupant.session_id).exists() {
        match claim_until_ready(state_dir, checkout, &occupant.session_id, true) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Ok(PresenceReconnect::ClaimLost(error));
            }
            Err(error) => return Err(error),
        }
    }
    Ok(PresenceReconnect::Connected(next))
}

/// Errors that no amount of retrying fixes while this process lives: a daemon
/// newer than this binary refuses it until the session restarts, and a mode
/// that may not spawn a daemon never gains that permission (#1981).
fn presence_error_is_permanent(error: &io::Error) -> bool {
    super::client::is_incompatible_daemon_error(error)
        || error.kind() == io::ErrorKind::PermissionDenied
}

/// Retry `attempt` with capped exponential backoff until it succeeds, `stop`
/// is set, or it fails permanently. Before #1981 this retried every 500 ms
/// forever, including against a newer daemon that refuses this binary for
/// good, and each attempt scanned the whole process table.
fn reconnect_with_backoff<T>(
    stop: &AtomicBool,
    mut attempt: impl FnMut() -> io::Result<T>,
    mut sleep: impl FnMut(Duration),
    mut jitter: impl FnMut(Duration) -> Duration,
) -> Result<T, ReconnectEnd> {
    let mut delay = PRESENCE_POLL_INTERVAL;
    while !stop.load(Ordering::SeqCst) {
        match attempt() {
            Ok(value) => return Ok(value),
            Err(error) if presence_error_is_permanent(&error) => {
                return Err(ReconnectEnd::Permanent(error));
            }
            Err(_) => {}
        }
        sleep(jitter(delay));
        delay = (delay * 2).min(PRESENCE_RECONNECT_CAP);
    }
    Err(ReconnectEnd::Stopped)
}

/// Spread `delay` over 75%..125% so sessions stranded by one daemon restart
/// do not reconnect in lockstep.
fn presence_jitter(delay: Duration) -> Duration {
    let mut byte = [0u8; 1];
    let _ = getrandom::fill(&mut byte);
    delay.mul_f64(0.75 + f64::from(byte[0]) / 510.0)
}

fn connect_presence(
    state_dir: &Path,
    checkout: &CheckoutKey,
    occupant: &CheckoutOccupant,
) -> io::Result<(TcpStream, Vec<CheckoutOccupant>)> {
    let (stream, reply) = checkout_rpc(
        state_dir,
        &CheckoutRequest::Present {
            checkout: checkout.clone(),
            occupant: occupant.clone(),
        },
    )?;
    match reply {
        CheckoutReply::Present { others } => Ok((stream, others)),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unexpected checkout presence reply",
        )),
    }
}

fn claim_until_ready(
    state_dir: &Path,
    checkout: &CheckoutKey,
    session_id: &str,
    restoring: bool,
) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let (_, reply) = checkout_rpc(
            state_dir,
            &CheckoutRequest::Claim {
                checkout: checkout.clone(),
                session_id: session_id.to_string(),
                restoring,
            },
        )?;
        match reply {
            CheckoutReply::Claimed => return Ok(()),
            CheckoutReply::Refused {
                reason,
                holder: None,
            } if reason.contains("grace period") && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(100));
            }
            CheckoutReply::Refused { reason, holder } => {
                let kind = if holder.is_some() {
                    io::ErrorKind::AlreadyExists
                } else {
                    io::ErrorKind::Other
                };
                let holder = holder.map_or_else(String::new, |occupant| {
                    format!(
                        "; held by session {} ({}{})",
                        occupant.session_id,
                        occupant.tool,
                        occupant
                            .run
                            .map_or_else(String::new, |run| format!(" {run}"))
                    )
                });
                return Err(io::Error::new(kind, format!(
                    "{reason}{holder}; choose a new worktree, wait for the claim to release, or cancel"
                )));
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unexpected checkout claim reply",
                ));
            }
        }
    }
}

fn checkout_rpc(
    state_dir: &Path,
    request: &CheckoutRequest,
) -> io::Result<(TcpStream, CheckoutReply)> {
    let info = read_json_file::<DaemonInfo>(&daemon_info_path(state_dir))?;
    let address = SocketAddr::from(([127, 0, 0, 1], info.port));
    let mut stream = TcpStream::connect_timeout(&address, CHECKOUT_RPC_TIMEOUT)?;
    stream.set_read_timeout(Some(CHECKOUT_RPC_TIMEOUT))?;
    stream.set_write_timeout(Some(CHECKOUT_RPC_TIMEOUT))?;
    serde_json::to_writer(&mut stream, request).map_err(io::Error::other)?;
    stream.write_all(b"\n")?;
    stream.flush()?;
    let mut line = String::new();
    BufReader::new(stream.try_clone()?).read_line(&mut line)?;
    let reply = serde_json::from_str(&line).map_err(io::Error::other)?;
    Ok((stream, reply))
}

fn occupants_in(
    state: &ClaimState,
    checkout: &CheckoutKey,
    exclude_session: Option<&str>,
) -> Vec<CheckoutOccupant> {
    let mut occupants = state
        .connections
        .values()
        .filter(|presence| {
            presence.checkout == *checkout
                && exclude_session != Some(presence.occupant.session_id.as_str())
        })
        .map(|presence| presence.occupant.clone())
        .collect::<Vec<_>>();
    occupants.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    occupants
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checkout(path: &str) -> CheckoutKey {
        CheckoutKey {
            worktree: PathBuf::from(path),
            common_git_dir: PathBuf::from("/repo/.git"),
        }
    }

    fn occupant(session_id: &str) -> CheckoutOccupant {
        CheckoutOccupant {
            session_id: session_id.to_string(),
            pid: 42,
            tool: "grind".to_string(),
            run: Some("#1342".to_string()),
            started_at: 1,
            claimed: false,
        }
    }

    #[test]
    fn claim_is_exclusive_and_disconnect_releases_it() {
        let registry = CheckoutClaimRegistry::new(Duration::ZERO);
        let key = checkout("/repo");
        registry.register(1, key.clone(), occupant("first"));
        registry.register(2, key.clone(), occupant("second"));
        assert_eq!(registry.claim("first", &key, false), Ok(()));
        assert_eq!(
            registry.claim("second", &key, false),
            Err(ClaimRefusal::HeldBy(CheckoutOccupant {
                claimed: true,
                ..occupant("first")
            }))
        );
        registry.disconnect(1);
        assert_eq!(registry.claim("second", &key, false), Ok(()));
    }

    #[test]
    fn worktrees_of_one_repo_are_independent() {
        let registry = CheckoutClaimRegistry::new(Duration::ZERO);
        let left = checkout("/repo");
        let right = checkout("/repo-worktree");
        registry.register(1, left.clone(), occupant("first"));
        registry.register(2, right.clone(), occupant("second"));
        assert_eq!(registry.claim("first", &left, false), Ok(()));
        assert_eq!(registry.claim("second", &right, false), Ok(()));
    }

    #[test]
    fn restart_grace_delays_new_claims_but_allows_restoration() {
        let registry = CheckoutClaimRegistry::new(Duration::from_secs(30));
        let key = checkout("/repo");
        registry.register(1, key.clone(), occupant("first"));
        assert_eq!(
            registry.claim("first", &key, false),
            Err(ClaimRefusal::StartupGrace)
        );
        assert_eq!(registry.claim("first", &key, true), Ok(()));
    }

    #[test]
    fn listing_contains_only_live_connections_and_release_clears_claim() {
        let registry = CheckoutClaimRegistry::new(Duration::ZERO);
        let key = checkout("/repo");
        registry.register(1, key.clone(), occupant("first"));
        registry.register(2, key.clone(), occupant("second"));
        registry.claim("first", &key, false).unwrap();
        assert_eq!(registry.all().len(), 2);
        assert!(registry.force_release("first", &key));
        assert!(registry
            .who(&key, None)
            .iter()
            .all(|person| !person.claimed));
        registry.disconnect(1);
        assert_eq!(registry.all().len(), 1);
        assert_eq!(registry.all()[0].1.session_id, "second");
    }

    fn newer_daemon_info(port: u16) -> DaemonInfo {
        let major = env!("CARGO_PKG_VERSION")
            .split('.')
            .next()
            .unwrap()
            .parse::<u64>()
            .unwrap();
        DaemonInfo {
            pid: std::process::id(),
            pid_start: crate::process_identity::self_start_time(),
            port,
            dashboard_port: None,
            dashboard_token: None,
            api_token: None,
            version: Some(format!("{}.0.0", major + 1)),
        }
    }

    /// #1981: a newer daemon refuses this binary for the rest of its life, so
    /// the presence thread gives up after one attempt instead of spinning.
    #[test]
    fn reconnect_stops_after_one_attempt_on_incompatible_daemon() {
        let stop = AtomicBool::new(false);
        let mut attempts = 0;
        let mut sleeps = 0;
        let result = reconnect_with_backoff::<()>(
            &stop,
            || {
                attempts += 1;
                Err(super::super::client::incompatible_daemon_error(
                    &newer_daemon_info(0),
                ))
            },
            |_| sleeps += 1,
            |delay| delay,
        );
        assert!(matches!(result, Err(ReconnectEnd::Permanent(ref error))
            if super::super::client::is_incompatible_daemon_error(error)));
        assert_eq!((attempts, sleeps), (1, 0));
    }

    #[test]
    fn reconnect_stops_on_permission_refusal() {
        let stop = AtomicBool::new(false);
        let mut attempts = 0;
        let result = reconnect_with_backoff::<()>(
            &stop,
            || {
                attempts += 1;
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "no spawn"))
            },
            |_| {},
            |delay| delay,
        );
        assert!(matches!(result, Err(ReconnectEnd::Permanent(_))));
        assert_eq!(attempts, 1);
    }

    /// #1981: transient failures back off exponentially up to the cap, so a
    /// minute of a dead daemon costs a handful of attempts, not 120.
    #[test]
    fn transient_failures_back_off_to_the_cap() {
        let stop = AtomicBool::new(false);
        let clock = std::cell::Cell::new(Duration::ZERO);
        let mut sleeps = Vec::new();
        let mut attempts_in_first_minute = 0;
        let result = reconnect_with_backoff(
            &stop,
            || {
                if clock.get() < Duration::from_secs(60) {
                    attempts_in_first_minute += 1;
                }
                if clock.get() >= Duration::from_secs(600) {
                    Ok("connected")
                } else {
                    Err(io::Error::from(io::ErrorKind::ConnectionRefused))
                }
            },
            |delay| {
                sleeps.push(delay);
                clock.set(clock.get() + delay);
            },
            |delay| delay,
        );
        assert_eq!(result.unwrap(), "connected");
        assert!(sleeps.windows(2).all(|pair| pair[1] >= pair[0]));
        assert_eq!(sleeps[0], PRESENCE_POLL_INTERVAL);
        assert_eq!(*sleeps.last().unwrap(), PRESENCE_RECONNECT_CAP);
        assert!(sleeps.iter().all(|delay| *delay <= PRESENCE_RECONNECT_CAP));
        assert!(
            attempts_in_first_minute <= 10,
            "{attempts_in_first_minute} attempts in 60 s"
        );
    }

    #[test]
    fn reconnect_honours_stop_between_attempts() {
        let stop = AtomicBool::new(false);
        let mut attempts = 0;
        let result = reconnect_with_backoff::<()>(
            &stop,
            || {
                attempts += 1;
                Err(io::Error::from(io::ErrorKind::ConnectionRefused))
            },
            |_| stop.store(true, Ordering::SeqCst),
            |delay| delay,
        );
        assert!(matches!(result, Err(ReconnectEnd::Stopped)));
        assert_eq!(attempts, 1);
    }

    #[test]
    fn jitter_stays_within_a_quarter_of_the_delay() {
        let delay = Duration::from_secs(4);
        for _ in 0..64 {
            let jittered = presence_jitter(delay);
            assert!(jittered >= Duration::from_secs(3) && jittered <= Duration::from_secs(5));
        }
    }

    /// #1981 upgrade scenario: an old session whose daemon was replaced by a
    /// newer build reconnects through the real `ensure_daemon`, gets refused,
    /// and disables presence after a single attempt.
    #[test]
    fn reconnect_against_newer_daemon_is_permanent() {
        let tmp = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let info = newer_daemon_info(listener.local_addr().unwrap().port());
        super::super::io_helpers::write_json_file(&daemon_info_path(tmp.path()), &info).unwrap();
        let stop = AtomicBool::new(false);
        let mut attempts = 0;
        let result = reconnect_with_backoff(
            &stop,
            || {
                attempts += 1;
                reconnect_presence(tmp.path(), &checkout("/repo"), &occupant("old"))
            },
            |_| panic!("a permanent refusal must not sleep and retry"),
            |delay| delay,
        );
        assert!(matches!(result, Err(ReconnectEnd::Permanent(_))));
        assert_eq!(attempts, 1);
    }

    /// #1981 upgrade scenario, end to end through the presence thread: the
    /// session holds a live presence connection, the daemon is replaced by a
    /// newer build, and the thread must disable presence after exactly one
    /// probe of the new daemon instead of probing it twice a second forever.
    #[test]
    fn presence_thread_disables_itself_when_a_newer_daemon_replaces_its_own() {
        let tmp = tempfile::tempdir().unwrap();
        let old_daemon = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let client = TcpStream::connect(old_daemon.local_addr().unwrap()).unwrap();
        let (old_connection, _) = old_daemon.accept().unwrap();

        let (ended_tx, ended_rx) = std::sync::mpsc::channel();
        let state_dir = tmp.path().to_path_buf();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let presence = thread::spawn(move || {
            let end = maintain_presence(
                client,
                &state_dir,
                &checkout("/repo"),
                &occupant("old"),
                &thread_stop,
            );
            ended_tx.send(end).unwrap();
        });

        // The upgrade: a newer daemon publishes itself, the old one goes away.
        let new_daemon = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let info = newer_daemon_info(new_daemon.local_addr().unwrap().port());
        super::super::io_helpers::write_json_file(&daemon_info_path(tmp.path()), &info).unwrap();
        drop(old_connection);
        drop(old_daemon);

        let end = ended_rx
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| {
                stop.store(true, Ordering::SeqCst);
                panic!("presence thread kept retrying against a newer daemon")
            });
        presence.join().unwrap();
        assert!(
            matches!(end, PresenceEnd::Disabled(ref error)
                if super::super::client::is_incompatible_daemon_error(error)),
            "{end:?}"
        );

        // The thread has exited, so it costs nothing from here on. Before the
        // fix it reconnected every 500 ms: count the new daemon's probes over
        // a few of those intervals to prove only the one attempt happened.
        thread::sleep(PRESENCE_POLL_INTERVAL * 4);
        new_daemon.set_nonblocking(true).unwrap();
        let probes = std::iter::from_fn(|| new_daemon.accept().ok()).count();
        assert_eq!(probes, 1, "the new daemon was probed {probes} times");
    }

    #[test]
    fn only_a_lost_claim_interrupts_the_session() {
        let interrupted = AtomicBool::new(false);
        assert_eq!(
            presence_end_notice(PresenceEnd::Stopped, &interrupted),
            None
        );
        let disabled = presence_end_notice(
            PresenceEnd::Disabled(io::Error::other("newer daemon")),
            &interrupted,
        );
        assert!(disabled.is_some_and(|notice| notice.contains("newer daemon")));
        assert!(!interrupted.load(Ordering::SeqCst));
        let lost = presence_end_notice(
            PresenceEnd::ClaimLost(io::Error::from(io::ErrorKind::AlreadyExists)),
            &interrupted,
        );
        assert!(lost.is_some_and(|notice| notice.contains("claim lost")));
        assert!(interrupted.load(Ordering::SeqCst));
    }
}

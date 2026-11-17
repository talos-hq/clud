//! Unix half of `crate::platform::remote_shell`: the `clud-remote-shell`
//! session leader and the client's private socket to it.
//!
//! The remote-assistance PTY's first process is `clud` running as
//! `clud-remote-shell <shell> [args...]`. portable-pty already made it the
//! session leader with the PTY as its controlling terminal. It connects back
//! to the client, makes itself non-dumpable, starts the user's shell as its
//! child in the same session, and then serves sudo requests
//! (`remote_connect::leader`) until the shell exits or the client goes away.
//!
//! The shell must inherit the PTY as stdin, stdout and stderr. running-process
//! `NativeProcess` always captures stdout, so the shell is started through
//! running-process's caller-owned foreground API with a configuration-only
//! `Command` (the `command-builder` exception in `ci/banned_imports.py`), and
//! reaped with `waitpid`. sudo itself runs through `NativeProcess` with pipes.

use crate::remote_connect::leader::{self, Leader, Reply, Request, Sudo};
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command; // running-process: command-builder
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// Whether this host can share a sudo ticket with the operator's shell.
pub const SUPPORTED: bool = true;

/// Where the setuid sudo is looked for (NixOS keeps its setuid wrappers in
/// `/run/wrappers/bin`). PATH is never consulted: the operator controls the
/// user's PATH and could plant a `sudo` that records passwords.
const SUDO_CANDIDATES: &[&str] = &["/usr/bin/sudo", "/bin/sudo", "/run/wrappers/bin/sudo"];
const POLL: Duration = Duration::from_millis(200);
const HANGUP_GRACE: Duration = Duration::from_secs(2);
const HANDSHAKE: Duration = Duration::from_secs(10);

/// What the leader runs: the user's shell and the sudo it trusts.
pub struct Config {
    pub shell: Vec<OsString>,
    pub sudo: Option<PathBuf>,
    pub keepalive: Duration,
    pub cap: Duration,
}

/// `clud-remote-shell <shell> [args...]`, the multicall entry.
pub fn run(argv: &[OsString]) -> i32 {
    let shell = argv.get(1..).unwrap_or_default().to_vec();
    if shell.is_empty() {
        eprintln!("usage: {} <shell> [args...]", leader::PROGRAM);
        return 2;
    }
    run_with(Config {
        shell,
        sudo: verified_sudo(),
        keepalive: leader::KEEPALIVE,
        cap: leader::CAP,
    })
}

/// The leader with an explicit configuration; tests pass a stub sudo.
pub fn run_with(config: Config) -> i32 {
    deny_inspection();
    let client = match connect_client() {
        Ok(client) => client,
        Err(error) => {
            eprintln!("{}: cannot reach the clud client: {error}", leader::PROGRAM);
            return 1;
        }
    };
    let stop = match install_signal_handlers() {
        Ok(stop) => stop,
        Err(error) => {
            eprintln!(
                "{}: cannot install signal handlers: {error}",
                leader::PROGRAM
            );
            return 1;
        }
    };
    let shell = match spawn_shell(&config.shell) {
        Ok(pid) => pid,
        Err(error) => {
            eprintln!("{}: cannot start the shell: {error}", leader::PROGRAM);
            return 127;
        }
    };
    let leader = Leader::new(config.sudo.map(Sudo::new), config.keepalive, config.cap);
    serve(client, shell, leader, &stop)
}

/// The same-uid operator must not read the password out of this process:
/// non-dumpable blocks ptrace and `/proc/<pid>/{mem,environ}` (Linux). On
/// macOS the signed release binary's hardened runtime refuses debuggers.
fn deny_inspection() {
    #[cfg(target_os = "linux")]
    // SAFETY: PR_SET_DUMPABLE takes plain integer arguments.
    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
    }
}

fn connect_client() -> io::Result<UnixStream> {
    let path = std::env::var_os(leader::SOCKET_ENV)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no client socket"))?;
    let nonce = std::env::var(leader::NONCE_ENV)
        .map_err(|_| io::Error::new(io::ErrorKind::NotFound, "no client nonce"))?;
    // std opens sockets close-on-exec, so the shell never inherits this one.
    let mut stream = UnixStream::connect(path)?;
    stream.write_all(format!("{nonce}\n").as_bytes())?;
    std::env::remove_var(leader::SOCKET_ENV);
    std::env::remove_var(leader::NONCE_ENV);
    // No shell exists until the client has verified this process: no
    // operator-controlled process can race the handshake.
    stream.set_read_timeout(Some(HANDSHAKE))?;
    let mut ack = zeroize::Zeroizing::new(Vec::new());
    if !leader::read_line(&mut stream, &mut ack)? || ack.as_slice() != leader::ACK {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "not accepted",
        ));
    }
    stream.set_read_timeout(None)?;
    Ok(stream)
}

/// SIGHUP/SIGTERM end the session. Tty signals get a no-op handler rather
/// than `SIG_IGN`: an ignored disposition survives `exec`, so the shell's
/// commands could not be interrupted, while a handler is reset to default.
fn install_signal_handlers() -> io::Result<Arc<AtomicBool>> {
    use signal_hook::consts::signal::{
        SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGTSTP, SIGTTIN, SIGTTOU,
    };
    let stop = Arc::new(AtomicBool::new(false));
    let ignored = Arc::new(AtomicBool::new(false));
    for signal in [SIGHUP, SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&stop))?;
    }
    for signal in [SIGINT, SIGQUIT, SIGTSTP, SIGTTIN, SIGTTOU] {
        signal_hook::flag::register(signal, Arc::clone(&ignored))?;
    }
    Ok(stop)
}

/// Start the shell in this session with the PTY as its stdio. No `setsid`
/// and no new process group: an interactive shell takes its own group.
fn spawn_shell(shell: &[OsString]) -> io::Result<libc::pid_t> {
    let mut command = Command::new(&shell[0]); // running-process: command-builder
    command.args(&shell[1..]);
    command.env_remove(leader::SOCKET_ENV);
    command.env_remove(leader::NONCE_ENV);
    let child = running_process::foreground::spawn(&mut command)?;
    libc::pid_t::try_from(child.id())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "pid out of range"))
}

fn serve(client: UnixStream, shell: libc::pid_t, mut leader: Leader, stop: &AtomicBool) -> i32 {
    let requests = read_requests(&client);
    let mut client = client;
    loop {
        match requests.recv_timeout(POLL) {
            Ok(request) => {
                let answer = with_tty_signals_ignored(|| leader.handle(request, Instant::now()));
                reply(&mut client, &answer);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            // The client is gone (Ctrl+C, crash, kill): end sudo and the shell.
            Err(mpsc::RecvTimeoutError::Disconnected) => return hang_up(&mut leader, shell),
        }
        if stop.load(Ordering::Relaxed) {
            return hang_up(&mut leader, shell);
        }
        if let Some(code) = reap(shell) {
            with_tty_signals_ignored(|| leader.revoke());
            return code;
        }
        if let Some(ended) = with_tty_signals_ignored(|| leader.tick(Instant::now())) {
            reply(&mut client, &ended);
        }
    }
}

/// Run `f`, which may start sudo, with SIGTTOU and SIGTTIN ignored. The
/// shell owns the terminal's foreground, so the leader and its children are
/// a background process group, and sudo-rs touches the controlling terminal
/// even with `-S`: the kernel stopped it with SIGTTOU. An ignored disposition
/// is inherited by sudo; the shell, started earlier, keeps the defaults.
fn with_tty_signals_ignored<T>(f: impl FnOnce() -> T) -> T {
    let saved = [libc::SIGTTOU, libc::SIGTTIN].map(|signal| {
        // SAFETY: zeroed `sigaction` values are valid to fill and to pass;
        // `ignore` names SIG_IGN with an empty mask.
        unsafe {
            let mut ignore: libc::sigaction = std::mem::zeroed();
            ignore.sa_sigaction = libc::SIG_IGN;
            libc::sigemptyset(&mut ignore.sa_mask);
            let mut previous: libc::sigaction = std::mem::zeroed();
            libc::sigaction(signal, &ignore, &mut previous);
            (signal, previous)
        }
    });
    let result = f();
    for (signal, previous) in saved {
        // SAFETY: restores exactly what `sigaction` returned above.
        unsafe {
            libc::sigaction(signal, &previous, std::ptr::null_mut());
        }
    }
    result
}

fn reply(client: &mut UnixStream, reply: &Reply) {
    // A lost client shows up as end of stream on the reader side.
    let _ = client.write_all(reply.line().as_bytes());
}

fn read_requests(client: &UnixStream) -> mpsc::Receiver<Request> {
    let (tx, rx) = mpsc::channel();
    let reader = client.try_clone();
    std::thread::spawn(move || {
        let Ok(mut reader) = reader else {
            return;
        };
        while let Ok(Some(request)) = leader::read_request(&mut reader) {
            if tx.send(request).is_err() {
                break;
            }
        }
    });
    rx
}

/// The shell's exit status as a PTY exit code, once it has exited.
fn reap(shell: libc::pid_t) -> Option<i32> {
    let mut status = 0;
    // SAFETY: `status` is a valid out-pointer; WNOHANG never blocks.
    let reaped = unsafe { libc::waitpid(shell, &mut status, libc::WNOHANG) };
    if reaped == 0 {
        return None;
    }
    if reaped < 0 {
        return Some(1);
    }
    if libc::WIFSIGNALED(status) {
        Some(128 + libc::WTERMSIG(status))
    } else {
        Some(libc::WEXITSTATUS(status))
    }
}

/// Drop sudo, then hang up the shell's process group and the shell.
fn hang_up(leader: &mut Leader, shell: libc::pid_t) -> i32 {
    with_tty_signals_ignored(|| leader.revoke());
    // SAFETY: signalling a pid this process started; a stale group is ESRCH.
    unsafe {
        libc::kill(-shell, libc::SIGHUP);
        libc::kill(shell, libc::SIGHUP);
    }
    let deadline = Instant::now() + HANGUP_GRACE;
    while Instant::now() < deadline {
        if reap(shell).is_some() {
            return 128 + libc::SIGHUP;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // SAFETY: as above.
    unsafe {
        libc::kill(-shell, libc::SIGKILL);
        libc::kill(shell, libc::SIGKILL);
    }
    let _ = reap(shell);
    128 + libc::SIGHUP
}

/// The first [`SUDO_CANDIDATES`] entry, resolved through symlinks, owned by root,
/// setuid, and not writable by group or others. The sudo-rs alternatives
/// symlink resolves to such a file; a user-planted `sudo` never qualifies.
pub fn verified_sudo() -> Option<PathBuf> {
    SUDO_CANDIDATES
        .iter()
        .filter_map(|candidate| std::fs::canonicalize(candidate).ok())
        .find(|path| trusted_setuid_root(path))
}

fn trusted_setuid_root(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|meta| {
        meta.is_file() && meta.uid() == 0 && meta.mode() & 0o4000 != 0 && meta.mode() & 0o022 == 0
    })
}

/// The client's end before the leader connects: a 0700 directory holding the
/// socket and the `clud-remote-shell` alias the PTY starts.
pub struct LeaderListener {
    dir: tempfile::TempDir,
    listener: UnixListener,
    nonce: String,
}

impl LeaderListener {
    pub fn bind() -> io::Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix("clud-remote-shell-")
            .tempdir()?;
        let listener = UnixListener::bind(dir.path().join("s"))?;
        let exe = std::env::current_exe()?;
        crate::alias_link::install_alias(&exe, &dir.path().join(leader::PROGRAM))?;
        let mut raw = [0u8; 16];
        getrandom::fill(&mut raw).map_err(|e| io::Error::other(e.to_string()))?;
        let nonce = raw.iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(Self {
            dir,
            listener,
            nonce,
        })
    }

    /// The PTY's program: this executable under the leader's name.
    pub fn program(&self) -> PathBuf {
        self.dir.path().join(leader::PROGRAM)
    }

    /// Environment the PTY needs so the leader can connect back.
    pub fn environment(&self) -> Vec<(String, String)> {
        vec![
            (
                leader::SOCKET_ENV.into(),
                self.dir.path().join("s").to_string_lossy().into_owned(),
            ),
            (leader::NONCE_ENV.into(), self.nonce.clone()),
        ]
    }

    /// Accept exactly one connection from the PTY's first process carrying
    /// the nonce, acknowledge it, then remove the socket and alias. Other
    /// connections are closed and ignored. `pid` reports the PTY's process:
    /// until acknowledged, the leader is its only process.
    pub fn accept(
        self,
        pid: &dyn Fn() -> Option<u32>,
        within: Duration,
    ) -> io::Result<LeaderStream> {
        self.listener.set_nonblocking(true)?;
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            match self.listener.accept() {
                Ok((mut stream, _)) => {
                    if let Some(peer) = self.authentic(&stream, pid) {
                        stream.write_all(leader::ACK)?;
                        stream.write_all(b"\n")?;
                        stream.set_read_timeout(None)?;
                        return Ok(LeaderStream { stream, pid: peer });
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "the remote shell leader did not connect",
        ))
    }

    /// The peer's pid when it is the PTY's session leader and knows the nonce.
    fn authentic(&self, stream: &UnixStream, pid: &dyn Fn() -> Option<u32>) -> Option<u32> {
        stream.set_nonblocking(false).ok()?;
        stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
        let peer = peer_pid(stream).ok()?;
        let expected = pid().and_then(|pid| libc::pid_t::try_from(pid).ok());
        // SAFETY: getsid only reads the session ID of `peer`.
        if expected != Some(peer) || unsafe { libc::getsid(peer) } != peer {
            return None;
        }
        let mut line = zeroize::Zeroizing::new(Vec::new());
        let mut reader = stream;
        let known = leader::read_line(&mut reader, &mut line).unwrap_or(false)
            && line.as_slice() == self.nonce.as_bytes();
        known.then(|| u32::try_from(peer).ok()).flatten()
    }
}

#[cfg(target_os = "linux")]
fn peer_pid(stream: &UnixStream) -> io::Result<libc::pid_t> {
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = libc::socklen_t::try_from(std::mem::size_of::<libc::ucred>())
        .map_err(|_| io::Error::other("ucred size"))?;
    // SAFETY: `cred` and `len` are valid out-pointers sized for SO_PEERCRED.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::addr_of_mut!(cred).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.pid)
}

#[cfg(not(target_os = "linux"))]
fn peer_pid(stream: &UnixStream) -> io::Result<libc::pid_t> {
    let mut pid: libc::pid_t = 0;
    let mut len = libc::socklen_t::try_from(std::mem::size_of::<libc::pid_t>())
        .map_err(|_| io::Error::other("pid size"))?;
    // SAFETY: `pid` and `len` are valid out-pointers sized for LOCAL_PEERPID.
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_LOCAL,
            libc::LOCAL_PEERPID,
            std::ptr::addr_of_mut!(pid).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(pid)
}

/// The client's authenticated connection to the leader.
pub struct LeaderStream {
    stream: UnixStream,
    pid: u32,
}

impl LeaderStream {
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            stream: self.stream.try_clone()?,
            pid: self.pid,
        })
    }

    /// The verified leader's process ID.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Bound how long a read may block, so shutdown never hangs.
    pub fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.stream.set_read_timeout(timeout)
    }
}

impl Read for LeaderStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.stream.read(buf)
    }
}

impl Write for LeaderStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "remote_shell_tests.rs"]
mod tests;

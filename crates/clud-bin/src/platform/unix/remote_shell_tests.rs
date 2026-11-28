//! The session leader in a real PTY session, driven the way the client
//! drives it: over its socket, with the operator typing into the PTY.
//!
//! The test binary re-runs itself as the PTY's first process
//! (`leader_child_entry`), so the leader is a real session leader with the
//! PTY as its controlling terminal. By default sudo is a stub that keys a
//! "ticket" by session ID, exactly as `timestamp_type=tty` does, and records
//! every invocation. With `CLUD_TEST_REAL_SUDO_PASSWORD` set (in a throwaway
//! container whose user has sudo rights) the same steps run against the
//! system's setuid sudo.

use super::*;
use running_process::pty::NativePtyProcess;
use std::sync::Arc;

const CHILD_ENV: &str = "CLUD_TEST_LEADER_CHILD";
const SUDO_ENV: &str = "CLUD_TEST_LEADER_SUDO";
const TIMING_ENV: &str = "CLUD_TEST_LEADER_TIMING_MS";
const REAL_PASSWORD_ENV: &str = "CLUD_TEST_REAL_SUDO_PASSWORD";
const STUB_PASSWORD: &str = "pw-SENTINEL-right";
const WAIT: Duration = Duration::from_secs(30);

/// Not a test of its own: the PTY child the other tests start.
#[test]
fn leader_child_entry() {
    if std::env::var_os(CHILD_ENV).is_none() {
        return;
    }
    let timing = std::env::var(TIMING_ENV).unwrap_or_default();
    let (keepalive, cap) = timing.split_once(',').expect("timing");
    let millis = |text: &str| Duration::from_millis(text.parse().expect("millis"));
    let code = run_with(Config {
        shell: vec!["/bin/sh".into()],
        sudo: std::env::var_os(SUDO_ENV).map(PathBuf::from),
        keepalive: millis(keepalive),
        cap: millis(cap),
    });
    std::process::exit(code);
}

const STUB: &str = r#"#!/bin/sh
state='@STATE@'
read -r _pid _comm _state _ppid _pgrp sid _rest < /proc/$$/stat
echo "argv=[$*] sid=$sid ppid=$PPID" >> "$state/log"
case "$*" in
  "-S -p  -v")
    IFS= read -r pw || pw=
    if [ "$pw" = '@PASSWORD@' ]; then echo on > "$state/ticket-$sid"; exit 0; fi
    echo "Sorry, try again." >&2
    echo "sudo: 1 incorrect password attempt" >&2
    exit 1 ;;
  "-n -v"|"-n true")
    ticket=off
    [ -f "$state/ticket-$sid" ] && read -r ticket < "$state/ticket-$sid"
    [ "$ticket" = on ] && exit 0
    echo "sudo: a password is required" >&2
    exit 1 ;;
  "-k") echo off > "$state/ticket-$sid"; exit 0 ;;
esac
echo "sudo: unexpected [$*]" >&2
exit 2
"#;

struct Fixture {
    dir: kernal_api::platform::fs::TemporaryDirectory,
    sudo: PathBuf,
    password: String,
    stub: bool,
}

impl Fixture {
    fn new() -> Self {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        if let Ok(password) = std::env::var(REAL_PASSWORD_ENV) {
            let sudo = verified_sudo().expect("a setuid root sudo for the real-sudo run");
            return Self {
                dir,
                sudo,
                password,
                stub: false,
            };
        }
        let state = dir.path().join("state");
        std::fs::create_dir(&state).unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let sudo = bin.join("sudo");
        let script = STUB
            .replace("@STATE@", &state.to_string_lossy())
            .replace("@PASSWORD@", STUB_PASSWORD);
        std::fs::write(&sudo, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&sudo, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            dir,
            sudo,
            password: STUB_PASSWORD.into(),
            stub: true,
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("state/log")).unwrap_or_default()
    }

    fn timing(&self) -> &'static str {
        // keepalive,cap in milliseconds. Real sudo refreshes are slower.
        if self.stub {
            "300,4000"
        } else {
            "1000,6000"
        }
    }
}

/// The PTY running the leader, plus the client's end of its socket.
struct Session {
    process: Arc<NativePtyProcess>,
    leader: LeaderStream,
    output: Vec<u8>,
    marks: usize,
}

impl Session {
    fn start(fixture: &Fixture) -> Self {
        let listener = LeaderListener::bind().unwrap();
        let mut env: Vec<(String, String)> = std::env::vars().collect();
        env.retain(|(key, _)| key != "PATH" && key != "ENV" && key != "HOME");
        let path = std::env::var("PATH").unwrap_or_default();
        let bin = fixture
            .sudo
            .parent()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        env.extend([
            ("PATH".into(), format!("{bin}:{path}")),
            (
                "HOME".into(),
                fixture.dir.path().to_string_lossy().into_owned(),
            ),
            ("PS1".into(), "$ ".into()),
            (CHILD_ENV.into(), "1".into()),
            (SUDO_ENV.into(), fixture.sudo.to_string_lossy().into_owned()),
            (TIMING_ENV.into(), fixture.timing().into()),
        ]);
        env.extend(listener.environment());
        let exe = std::env::current_exe().unwrap();
        let argv = vec![
            exe.to_string_lossy().into_owned(),
            "--exact".into(),
            "platform::unix::remote_shell::tests::leader_child_entry".into(),
            "--nocapture".into(),
            "--test-threads=1".into(),
        ];
        let process =
            Arc::new(NativePtyProcess::new(argv, None, Some(env), 24, 120, None).unwrap());
        process.start_impl().unwrap();
        let pid = || process.pid().ok().flatten();
        let leader = listener.accept(&pid, WAIT).expect("leader connects");
        leader.set_read_timeout(Some(WAIT)).unwrap();
        Self {
            process,
            leader,
            output: Vec::new(),
            marks: 0,
        }
    }

    fn ask(&mut self, request: &[u8]) -> Reply {
        self.leader.write_all(request).unwrap();
        self.reply()
    }

    fn validate(&mut self, password: &str) -> Reply {
        leader::write_validate(&mut self.leader, password.as_bytes()).unwrap();
        self.reply()
    }

    fn reply(&mut self) -> Reply {
        let mut line = zeroize::Zeroizing::new(Vec::new());
        assert!(
            leader::read_line(&mut self.leader, &mut line).unwrap(),
            "leader closed"
        );
        Reply::parse(std::str::from_utf8(&line).unwrap()).expect("a reply line")
    }

    /// Type `command` into the operator's shell; return its `RC=<status>`.
    fn operator(&mut self, command: &str) -> String {
        self.marks += 1;
        let mark = format!(" DONE_{}\r", self.marks);
        let line = format!("{command}; echo \"RC=$? DONE_{}\"\n", self.marks);
        self.process.write_impl(line.as_bytes(), false).unwrap();
        let start = self.output.len();
        let deadline = Instant::now() + WAIT;
        loop {
            let text = String::from_utf8_lossy(&self.output[start..]).into_owned();
            if let Some(end) = text.find(&mark) {
                let begin = text[..end].rfind("RC=").expect("status before the mark");
                return text[begin..end].to_owned();
            }
            assert!(Instant::now() < deadline, "no{mark:?}: {text}");
            if let Ok(Some(bytes)) = self.process.read_chunk_impl(Some(0.1)) {
                self.output.extend_from_slice(&bytes);
            }
        }
    }

    fn sudo_rc(&mut self) -> String {
        self.operator("sudo -n true 2>/dev/null")
    }

    fn exit_code(&self) -> Option<i32> {
        self.process.wait_impl(Some(10.0)).ok()
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[test]
fn request_drop_keepalive_and_expiry_in_a_real_pty_session() {
    let fixture = Fixture::new();
    let mut session = Session::start(&fixture);
    assert_eq!(session.sudo_rc(), "RC=1", "no ticket before validation");
    assert_eq!(session.ask(b"probe\n"), Reply::NeedsPassword);
    assert_eq!(session.validate("not-the-password"), Reply::Wrong);
    assert_eq!(session.sudo_rc(), "RC=1");

    let Reply::Active { until } = session.validate(&fixture.password) else {
        panic!("validation failed");
    };
    let cap_secs = if fixture.stub { 4 } else { 6 };
    assert!(until.abs_diff(now_secs() + cap_secs) <= 2, "until={until}");
    assert_eq!(
        session.sudo_rc(),
        "RC=0",
        "the operator's shell shares the ticket"
    );
    // From a script too: a different parent, the same session.
    let nested = session.operator("sh -c 'sudo -n true' 2>/dev/null");
    assert_eq!(nested, "RC=0");

    // The shell never inherited the socket to the client.
    let fds = session.operator("ls -l /proc/$$/fd | grep -c socket");
    assert_eq!(fds, "RC=1", "a socket leaked into the shell");

    assert_eq!(session.ask(b"drop\n"), Reply::Dropped);
    assert_eq!(session.sudo_rc(), "RC=1", "drop ends the ticket");

    assert!(matches!(
        session.validate(&fixture.password),
        Reply::Active { .. }
    ));
    assert_eq!(session.sudo_rc(), "RC=0");
    // Keep-alive runs until the cap, then the leader ends the ticket itself.
    assert_eq!(session.reply(), Reply::Expired);
    assert_eq!(session.sudo_rc(), "RC=1", "expired at the cap");

    session.process.write_impl(b"exit 3\n", false).unwrap();
    assert_eq!(
        session.exit_code(),
        Some(3),
        "the leader exits with the shell's code"
    );

    assert_nothing_leaked(&fixture, &session);
}

/// Neither the password nor sudo's output reached the PTY; with the stub,
/// sudo saw the expected calls and never `-K`.
fn assert_nothing_leaked(fixture: &Fixture, session: &Session) {
    let shown = String::from_utf8_lossy(&session.output).into_owned();
    assert!(
        !shown.contains(&fixture.password),
        "password reached the PTY"
    );
    assert!(
        !shown.contains("password is required"),
        "sudo text reached the PTY"
    );
    if fixture.stub {
        let log = fixture.log();
        assert!(!log.contains(&fixture.password), "{log}");
        assert_eq!(log.matches("argv=[-S -p  -v]").count(), 3, "{log}");
        assert!(
            log.matches("argv=[-n -v]").count() >= 4,
            "keep-alive ran: {log}"
        );
        assert!(
            log.matches("argv=[-k]").count() >= 3,
            "drop, expiry, exit: {log}"
        );
        assert!(!log.contains("-K"), "{log}");
    }
}

#[test]
fn losing_the_client_drops_sudo_and_hangs_up_the_shell() {
    let fixture = Fixture::new();
    let mut session = Session::start(&fixture);
    assert!(matches!(
        session.validate(&fixture.password),
        Reply::Active { .. }
    ));
    assert_eq!(session.sudo_rc(), "RC=0");
    let pid = session.leader.pid();
    // SAFETY: plain getuid.
    if unsafe { libc::getuid() } != 0 {
        let environ = std::fs::read(format!("/proc/{pid}/environ"));
        assert!(environ.is_err(), "the leader is dumpable");
    }
    let Session {
        process, leader, ..
    } = session;
    drop(leader);
    assert_eq!(process.wait_impl(Some(10.0)).ok(), Some(128 + libc::SIGHUP));
    if fixture.stub {
        assert!(fixture
            .log()
            .trim_end()
            .ends_with(&format!("argv=[-k] sid={pid} ppid={pid}")));
    }
}

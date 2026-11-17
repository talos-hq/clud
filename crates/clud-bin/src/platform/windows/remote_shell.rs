//! Windows half of `crate::platform::remote_shell`: not supported.
//!
//! Windows has no credential ticket a second process could share: UAC
//! elevates one process at a time, and an unelevated ConPTY shell cannot
//! become elevated. The client therefore never advertises `sudo` here, and
//! the operator UI disables the button. A UAC-elevated admin tab is tracked
//! separately (see `docs/architecture/remote-assistance.md#request-sudo`).

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

pub const SUPPORTED: bool = false;

pub struct Config {
    pub shell: Vec<OsString>,
    pub sudo: Option<PathBuf>,
    pub keepalive: Duration,
    pub cap: Duration,
}

pub fn run(_argv: &[OsString]) -> i32 {
    eprintln!("clud-remote-shell is not supported on Windows");
    2
}

pub fn run_with(_config: Config) -> i32 {
    run(&[])
}

pub fn verified_sudo() -> Option<PathBuf> {
    None
}

fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "remote sudo is not supported on Windows",
    )
}

pub enum LeaderListener {}

impl LeaderListener {
    pub fn bind() -> io::Result<Self> {
        Err(unsupported())
    }

    pub fn program(&self) -> PathBuf {
        match *self {}
    }

    pub fn environment(&self) -> Vec<(String, String)> {
        match *self {}
    }

    pub fn accept(
        self,
        _pid: &dyn Fn() -> Option<u32>,
        _within: Duration,
    ) -> io::Result<LeaderStream> {
        match self {}
    }
}

pub enum LeaderStream {}

impl LeaderStream {
    pub fn try_clone(&self) -> io::Result<Self> {
        match *self {}
    }

    pub fn pid(&self) -> u32 {
        match *self {}
    }

    pub fn set_read_timeout(&self, _timeout: Option<Duration>) -> io::Result<()> {
        match *self {}
    }
}

impl Read for LeaderStream {
    fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
        match *self {}
    }
}

impl Write for LeaderStream {
    fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
        match *self {}
    }

    fn flush(&mut self) -> io::Result<()> {
        match *self {}
    }
}

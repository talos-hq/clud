//! TLS for the non-blocking session socket.
//!
//! rustls's own `Stream` wrapper suits a blocking socket. On a non-blocking
//! one, two of its behaviours break a session on a congested uplink
//! (`docs/architecture/remote-assistance.md#a-slow-link-is-waited-out`):
//! - Its send buffer is capped (64 KiB by default). When the cap is reached a
//!   write is accepted as `Ok(0)`, which tungstenite reports as
//!   `ConnectionReset`, so a model request bigger than the cap ended the
//!   connection whenever the network was full.
//! - A read first finishes every pending write, so while the uplink is full
//!   the operator's keystrokes and the server's pongs, already received and
//!   decrypted, are held back until the heartbeat declares the link dead.
//!
//! This stream accepts every write (the session loop already stops sending
//! while anything is queued, so the buffer stays small), sends what it can
//! without letting a full uplink fail the call, and reads whatever has
//! arrived whether or not writes are stuck. `WouldBlock` from `flush` means
//! records are still queued; from `read`, that nothing has arrived.
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use ureq::rustls::pki_types::ServerName;
use ureq::rustls::{ClientConfig, ClientConnection};

/// A TLS client connection over `S` (a TCP stream; a test double in tests).
pub(super) struct TlsStream<S> {
    conn: ClientConnection,
    sock: S,
}

impl<S: Read + Write> TlsStream<S> {
    /// A client for `host` over `sock`. The handshake runs on first use, with
    /// whatever blocking or timeouts `sock` has.
    pub(super) fn new(config: Arc<ClientConfig>, host: &str, sock: S) -> io::Result<Self> {
        let name = ServerName::try_from(host.to_owned())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid TLS server name"))?;
        let mut conn = ClientConnection::new(config, name).map_err(io::Error::other)?;
        // Unbounded: a capped buffer turns a full uplink into `Ok(0)`, a reset.
        conn.set_buffer_limit(None);
        Ok(Self { conn, sock })
    }

    pub(super) fn get_ref(&self) -> &S {
        &self.sock
    }

    /// Finish the handshake first: before it, nothing else can move.
    fn handshake(&mut self) -> io::Result<()> {
        while self.conn.is_handshaking() {
            self.conn.complete_io(&mut self.sock)?;
        }
        Ok(())
    }

    /// Send queued records until they are all out or the network is full.
    fn send_records(&mut self) -> io::Result<()> {
        while self.conn.wants_write() {
            if self.conn.write_tls(&mut self.sock)? == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
        }
        Ok(())
    }

    /// [`Self::send_records`], where a full network is not a failure.
    fn send_records_if_possible(&mut self) -> io::Result<()> {
        match self.send_records() {
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
            other => other,
        }
    }
}

impl<S: Read + Write> Read for TlsStream<S> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.handshake()?;
        // Alerts and key updates go out when they can; a full uplink must not
        // hold back what has already arrived.
        self.send_records_if_possible()?;
        loop {
            match self.conn.reader().read(buf) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                // Data, a clean close (0), or an unclean end of the stream.
                done => return done,
            }
            // Nothing decrypted is waiting: take more from the network.
            // `WouldBlock` here means nothing has arrived.
            self.conn.read_tls(&mut self.sock)?;
            self.conn
                .process_new_packets()
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        }
    }
}

impl<S: Read + Write> Write for TlsStream<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.handshake()?;
        let taken = self.conn.writer().write(buf)?;
        self.send_records_if_possible()?;
        Ok(taken)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.handshake()?;
        self.conn.writer().flush()?;
        self.send_records()
    }
}

/// The session socket's byte stream: TLS for `wss://`, plain for loopback `ws://`.
pub(super) enum Stream {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

impl Stream {
    pub(super) fn tcp(&self) -> &TcpStream {
        match self {
            Self::Plain(tcp) => tcp,
            Self::Tls(tls) => tls.get_ref(),
        }
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(tcp) => tcp.read(buf),
            Self::Tls(tls) => tls.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(tcp) => tcp.write(buf),
            Self::Tls(tls) => tls.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(tcp) => tcp.flush(),
            Self::Tls(tls) => tls.flush(),
        }
    }
}

#[cfg(test)]
#[path = "tls_tests.rs"]
mod tests;

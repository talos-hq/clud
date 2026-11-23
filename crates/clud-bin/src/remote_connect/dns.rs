//! Limit how long a dial waits for DNS without spawning a resolver on every retry.
use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::{mpsc, Mutex};
use std::time::Duration;

type Answer = io::Result<Vec<SocketAddr>>;

#[derive(Debug, Default)]
pub(super) struct Resolver {
    pending: Mutex<Option<mpsc::Receiver<Answer>>>,
}

impl Resolver {
    pub(super) fn resolve(&self, host: &str, port: u16, wait: Duration) -> Answer {
        let host = host.to_owned();
        self.resolve_with(wait, move || {
            (host.as_str(), port)
                .to_socket_addrs()
                .map(Iterator::collect)
        })
    }

    fn resolve_with(
        &self,
        wait: Duration,
        lookup: impl FnOnce() -> Answer + Send + 'static,
    ) -> Answer {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if pending.is_none() {
            let (send, receive) = mpsc::sync_channel(1);
            std::thread::Builder::new()
                .name("clud-relay-dns".into())
                .spawn(move || {
                    let _ = send.send(lookup());
                })?;
            *pending = Some(receive);
        }
        match pending.as_ref().unwrap().recv_timeout(wait) {
            Ok(answer) => {
                *pending = None;
                answer
            }
            // The platform resolver is not cancellable. Keep the receiver so
            // another retry uses the same lookup until it finishes.
            Err(mpsc::RecvTimeoutError::Timeout) => Err(io::ErrorKind::TimedOut.into()),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                *pending = None;
                Err(io::ErrorKind::BrokenPipe.into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timed_out_lookup_is_reused_then_refreshed() {
        let resolver = Resolver::default();
        let old = "192.0.2.1:443".parse().unwrap();
        let new = "[2001:db8::1]:443".parse().unwrap();
        let (release, wait) = mpsc::channel();
        let timed_out = resolver.resolve_with(Duration::ZERO, move || {
            wait.recv().unwrap();
            Ok(vec![old])
        });
        assert_eq!(timed_out.unwrap_err().kind(), io::ErrorKind::TimedOut);
        release.send(()).unwrap();
        assert_eq!(
            resolver
                .resolve_with(Duration::from_secs(2), || panic!("duplicate lookup"))
                .unwrap(),
            [old]
        );
        assert_eq!(
            resolver
                .resolve_with(Duration::from_secs(2), move || Ok(vec![new]))
                .unwrap(),
            [new]
        );
    }
}

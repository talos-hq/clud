//! Limit how long a dial waits for DNS without spawning a resolver on every retry.
use std::io;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::{mpsc, Mutex};
use std::time::Duration;

type Answer = io::Result<Vec<SocketAddr>>;

#[derive(Debug, Default)]
pub(super) struct Resolver {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    pending: Option<mpsc::Receiver<Answer>>,
    previous: Vec<SocketAddr>,
}

impl Resolver {
    /// Try current DNS first. The verified peer takes precedence over
    /// in-process DNS answers from an earlier request when DNS is down.
    pub(super) fn with_fallback(
        &self,
        host: &str,
        port: u16,
        wait: Duration,
        peer: Option<SocketAddr>,
    ) -> Answer {
        let host = host.to_owned();
        self.with_fallback_using(wait, peer, move || {
            (host.as_str(), port)
                .to_socket_addrs()
                .map(Iterator::collect)
        })
    }

    fn with_fallback_using(
        &self,
        wait: Duration,
        peer: Option<SocketAddr>,
        lookup: impl FnOnce() -> Answer + Send + 'static,
    ) -> Answer {
        let fresh = self.resolve_with(wait, lookup);
        if fresh.as_ref().is_ok_and(|addresses| !addresses.is_empty()) {
            return fresh;
        }
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let fallback = peer
            .into_iter()
            .chain(state.previous.iter().copied())
            .collect::<Vec<_>>();
        if fallback.is_empty() {
            return fresh;
        }
        Ok(fallback)
    }

    fn resolve_with(
        &self,
        wait: Duration,
        lookup: impl FnOnce() -> Answer + Send + 'static,
    ) -> Answer {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.pending.is_none() {
            let (send, receive) = mpsc::sync_channel(1);
            std::thread::Builder::new()
                .name("clud-relay-dns".into())
                .spawn(move || {
                    let _ = send.send(lookup());
                })?;
            state.pending = Some(receive);
        }
        match state.pending.as_ref().unwrap().recv_timeout(wait) {
            Ok(answer) => {
                state.pending = None;
                if let Ok(addresses) = &answer {
                    if !addresses.is_empty() {
                        state.previous.clone_from(addresses);
                    }
                }
                answer
            }
            // The platform resolver is not cancellable. Keep the receiver so
            // another retry uses the same lookup until it finishes.
            Err(mpsc::RecvTimeoutError::Timeout) => Err(io::ErrorKind::TimedOut.into()),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                state.pending = None;
                Err(io::ErrorKind::BrokenPipe.into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_prior_answer_survives_dns_failure_between_http_and_websocket() {
        let resolver = Resolver::default();
        let address = "192.0.2.1:443".parse().unwrap();
        let verified = "[2001:db8::1]:443".parse().unwrap();
        let wait = Duration::from_secs(2);
        assert_eq!(
            resolver
                .with_fallback_using(wait, None, move || Ok(vec![address]))
                .unwrap(),
            [address]
        );
        assert_eq!(
            resolver
                .with_fallback_using(wait, Some(verified), || Err(io::ErrorKind::NotFound.into()))
                .unwrap(),
            [verified, address]
        );
        assert_eq!(
            resolver
                .with_fallback_using(wait, None, || Ok(vec![]))
                .unwrap(),
            [address]
        );
    }

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

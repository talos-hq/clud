//! A verified relay address can bootstrap the next Clud process during DNS loss.
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use url::Url;

const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const MAX_BYTES: u64 = 256;

#[derive(Deserialize, Serialize)]
struct Record {
    peer: SocketAddr,
    seen: u64,
}

pub(super) struct PeerCache(PathBuf);

impl PeerCache {
    /// Persist only the built-in relay's secure, default-port origin.
    pub(super) fn for_url(url: &Url) -> Option<Self> {
        if matches!(url.scheme(), "https" | "wss")
            && url.host_str() == Some("clud.bot")
            && url.port_or_known_default() == Some(443)
        {
            return Some(Self(
                crate::home::user_home()?.join(".clud/cache/remote-peer.json"),
            ));
        }
        None
    }

    pub(super) fn load(&self) -> Option<SocketAddr> {
        self.load_at(SystemTime::now())
    }

    fn load_at(&self, now: SystemTime) -> Option<SocketAddr> {
        if !fs::metadata(&self.0).ok()?.is_file() {
            return None;
        }
        let mut bytes = Vec::new();
        File::open(&self.0)
            .ok()?
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() as u64 > MAX_BYTES {
            return None;
        }
        let record: Record = serde_json::from_slice(&bytes).ok()?;
        let age = now
            .duration_since(UNIX_EPOCH)
            .ok()?
            .as_secs()
            .checked_sub(record.seen)?;
        (age <= MAX_AGE.as_secs() && record.peer.port() == 443).then_some(record.peer)
    }

    pub(super) fn remember(&self, peer: SocketAddr) {
        let _ = self.store_at(peer, SystemTime::now());
    }

    fn store_at(&self, peer: SocketAddr, now: SystemTime) -> io::Result<()> {
        let directory = self.0.parent().ok_or(io::ErrorKind::InvalidInput)?;
        fs::create_dir_all(directory)?;
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        let seen = now
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_secs();
        serde_json::to_writer(&mut temporary, &Record { peer, seen })?;
        temporary.flush()?;
        temporary.persist(&self.0)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_builtin_secure_origin_uses_the_persistent_cache() {
        for url in ["https://clud.bot/", "wss://clud.bot:443/ws/client"] {
            assert!(PeerCache::for_url(&Url::parse(url).unwrap()).is_some());
        }
        for url in [
            "http://clud.bot/",
            "https://clud.bot:444/",
            "https://other.test/",
        ] {
            assert!(PeerCache::for_url(&Url::parse(url).unwrap()).is_none());
        }
    }

    #[test]
    fn a_new_process_can_load_a_current_peer_but_ignores_bad_records() {
        let directory = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let path = directory.path().join("remote-peer.json");
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        let peer = "[2001:db8::1]:443".parse().unwrap();
        PeerCache(path.clone()).store_at(peer, now).unwrap();
        let restarted = PeerCache(path.clone());
        assert_eq!(restarted.load_at(now), Some(peer));
        assert_eq!(restarted.load_at(now - Duration::from_secs(1)), None);
        assert_eq!(
            restarted.load_at(now + MAX_AGE + Duration::from_secs(1)),
            None
        );
        restarted
            .store_at("192.0.2.1:444".parse().unwrap(), now)
            .unwrap();
        assert_eq!(restarted.load_at(now), None);
        fs::write(path, b"{corrupt}").unwrap();
        assert_eq!(restarted.load_at(now), None);
    }
}

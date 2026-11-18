//! End-to-end encryption of the session socket, inside TLS.
//!
//! clud.bot's hosting edge terminates TLS, so TLS alone lets it read (and its
//! firewall inspect) every frame. Each connection therefore runs its own key
//! exchange: both sides make an ephemeral X25519 key pair, swap public keys
//! and nonces in plaintext, and derive one ChaCha20-Poly1305 key per direction
//! with HKDF-SHA256 over the shared secret, salted with a hash of the
//! transcript. Every application frame after that is a sealed record; nonces
//! are per-direction counters, so a replayed, reordered, truncated or altered
//! record fails authentication and ends the session.
//!
//! The exchange is not authenticated (no server identity key, by design): it
//! defeats an edge that reads or inspects traffic, not one that actively
//! substitutes its own keys. See DD-219.
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use x25519_dalek::{EphemeralSecret, PublicKey};
use zeroize::Zeroizing;

/// Protocol version, offered by the server and echoed by the client.
pub(super) const VERSION: u64 = 1;
pub(super) const SUITE: &str = "x25519-hkdf-sha256-chacha20poly1305";
/// First byte of every sealed record; also its associated data.
const RECORD: u8 = 0xE2;
/// Inner frame kinds: a JSON text event or a binary (PTY / model) frame.
pub(super) const TEXT: u8 = 0;
pub(super) const BINARY: u8 = 1;
/// The close code both peers send when a record fails authentication.
pub(super) const FAILED_CLOSE: u16 = 4400;
pub(super) const FAILED: &str =
    "an encrypted frame from clud.bot failed authentication; the session has ended";
pub(super) const DOWNGRADE: &str =
    "clud.bot did not offer end-to-end encryption; refusing to continue in plaintext";
const TRANSCRIPT_LABEL: &[u8] = b"clud.bot e2e transcript";
const CLIENT_TO_SERVER: &[u8] = b"clud.bot e2e v1 client to server";
const SERVER_TO_CLIENT: &[u8] = b"clud.bot e2e v1 server to client";
const TAG: usize = 16;

/// The server's half of the exchange (`e2e_offer`).
pub(super) struct Offer {
    key: [u8; 32],
    nonce: [u8; 32],
}

fn bytes32(value: &Value) -> Option<[u8; 32]> {
    STANDARD.decode(value.as_str()?).ok()?.try_into().ok()
}

/// `Ok(None)` for any first event that is not an offer (a server without
/// encryption greets with `hello`); `Err` for an offer this client cannot use.
pub(super) fn parse_offer(event: &Value) -> Result<Option<Offer>, String> {
    if event["type"] != "e2e_offer" {
        return Ok(None);
    }
    if event["v"].as_u64() != Some(VERSION) || event["suite"] != SUITE {
        return Err("clud.bot offered an unsupported end-to-end encryption version".into());
    }
    match (bytes32(&event["key"]), bytes32(&event["nonce"])) {
        (Some(key), Some(nonce)) => Ok(Some(Offer { key, nonce })),
        _ => Err("clud.bot sent a malformed end-to-end encryption offer".into()),
    }
}

/// The client's ephemeral key pair, used once.
pub(super) struct Handshake {
    secret: EphemeralSecret,
    public: PublicKey,
    nonce: [u8; 32],
}

impl Handshake {
    pub(super) fn new() -> Result<Self, String> {
        let mut nonce = [0_u8; 32];
        getrandom::fill(&mut nonce).map_err(|_| "no system randomness for encryption")?;
        let secret = EphemeralSecret::random();
        let public = PublicKey::from(&secret);
        Ok(Self {
            secret,
            public,
            nonce,
        })
    }

    /// The client's reply to the offer, sent in plaintext.
    pub(super) fn accept(&self) -> Value {
        json!({
            "type": "e2e_accept",
            "v": VERSION,
            "key": STANDARD.encode(self.public.as_bytes()),
            "nonce": STANDARD.encode(self.nonce),
        })
    }

    /// Derive the session's keys. Consumes (and so wipes) the secret.
    pub(super) fn finish(self, session_id: &str, offer: &Offer) -> Result<Channel, String> {
        let shared = self.secret.diffie_hellman(&PublicKey::from(offer.key));
        // A low-order server key forces a known shared secret.
        if !shared.was_contributory() {
            return Err("clud.bot sent an invalid encryption key".into());
        }
        let salt = transcript(&Transcript {
            session_id,
            server_key: &offer.key,
            server_nonce: &offer.nonce,
            client_key: self.public.as_bytes(),
            client_nonce: &self.nonce,
        });
        let (send, recv) = derive(shared.as_bytes(), &salt)?;
        Ok(Channel::new(send, recv))
    }
}

/// Everything both sides saw in the exchange; any difference yields different keys.
pub(super) struct Transcript<'a> {
    pub session_id: &'a str,
    pub server_key: &'a [u8; 32],
    pub server_nonce: &'a [u8; 32],
    pub client_key: &'a [u8; 32],
    pub client_nonce: &'a [u8; 32],
}

/// SHA-256 over length-prefixed fields, in this fixed order.
pub(super) fn transcript(parts: &Transcript<'_>) -> [u8; 32] {
    let version = VERSION.to_be_bytes();
    let mut hash = Sha256::new();
    hash.update(TRANSCRIPT_LABEL);
    for field in [
        &version[..],
        SUITE.as_bytes(),
        parts.session_id.as_bytes(),
        parts.server_key,
        parts.server_nonce,
        parts.client_key,
        parts.client_nonce,
    ] {
        let length = u32::try_from(field.len()).unwrap_or(u32::MAX);
        hash.update(length.to_be_bytes());
        hash.update(field);
    }
    hash.finalize().into()
}

type KeyBytes = Zeroizing<[u8; 32]>;

/// (client-to-server, server-to-client) keys.
pub(super) fn derive(shared: &[u8; 32], salt: &[u8; 32]) -> Result<(KeyBytes, KeyBytes), String> {
    let hkdf = Hkdf::<Sha256>::new(Some(salt), shared);
    let mut send = Zeroizing::new([0_u8; 32]);
    let mut recv = Zeroizing::new([0_u8; 32]);
    hkdf.expand(CLIENT_TO_SERVER, send.as_mut())
        .and_then(|()| hkdf.expand(SERVER_TO_CLIENT, recv.as_mut()))
        .map_err(|_| "encryption key derivation failed")?;
    Ok((send, recv))
}

/// One direction: its key and the counter that is the next record's nonce.
struct Direction {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl Direction {
    fn new(key: &KeyBytes) -> Self {
        Self {
            cipher: ChaCha20Poly1305::new(Key::from_slice(key.as_ref())),
            counter: 0,
        }
    }

    /// The next nonce; `None` once the counter is spent (never in practice).
    fn nonce(&mut self) -> Option<Nonce> {
        let counter = self.counter;
        self.counter = counter.checked_add(1)?;
        let mut nonce = [0_u8; 12];
        nonce[4..].copy_from_slice(&counter.to_be_bytes());
        Some(Nonce::from(nonce))
    }
}

/// The established channel. Keys are wiped when it is dropped.
pub(super) struct Channel {
    send: Direction,
    recv: Direction,
}

impl Channel {
    pub(super) fn new(send: KeyBytes, recv: KeyBytes) -> Self {
        Self {
            send: Direction::new(&send),
            recv: Direction::new(&recv),
        }
    }

    /// `RECORD | ChaCha20-Poly1305(kind | payload)`.
    pub(super) fn seal(&mut self, kind: u8, payload: &[u8]) -> Result<Vec<u8>, String> {
        let nonce = self.send.nonce().ok_or("encrypted channel exhausted")?;
        let mut inner = Zeroizing::new(Vec::with_capacity(1 + payload.len()));
        inner.push(kind);
        inner.extend_from_slice(payload);
        let sealed = self
            .send
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: &inner,
                    aad: &[RECORD],
                },
            )
            .map_err(|_| "encryption failed")?;
        let mut record = Vec::with_capacity(1 + sealed.len());
        record.push(RECORD);
        record.extend_from_slice(&sealed);
        Ok(record)
    }

    /// The inner kind and payload. Any failure is final: the caller closes.
    pub(super) fn open(&mut self, record: &[u8]) -> Result<(u8, Vec<u8>), ()> {
        let (&first, sealed) = record.split_first().ok_or(())?;
        if first != RECORD || sealed.len() < TAG + 1 {
            return Err(());
        }
        let nonce = self.recv.nonce().ok_or(())?;
        let mut inner = self
            .recv
            .cipher
            .decrypt(
                &nonce,
                Payload {
                    msg: sealed,
                    aad: &[RECORD],
                },
            )
            .map_err(|_| ())?;
        let kind = inner.remove(0);
        if kind != TEXT && kind != BINARY {
            return Err(());
        }
        Ok((kind, inner))
    }
}

#[cfg(test)]
#[path = "e2e_tests.rs"]
mod tests;

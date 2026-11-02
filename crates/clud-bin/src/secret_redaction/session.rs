//! In-memory token ownership and reversible JSON/text transforms.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::OnceLock;

use hmac::{Hmac, Mac};
use zeroize::{Zeroize, Zeroizing};

use super::detect;

const PREFIX: &str = "clud_secret_";
const TOKEN_LENGTH: usize = 24;

#[derive(Default, Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Counts {
    pub kinds: BTreeMap<String, u64>,
    pub unknown_tokens: u64,
}

/// No Debug, Clone or serialization surface exposes session key material.
pub struct Session {
    key: Zeroizing<[u8; 32]>,
    known: Vec<Zeroizing<String>>,
    mapping: BTreeMap<String, Zeroizing<String>>,
    counts: Counts,
}

impl Session {
    pub fn new(known: Vec<String>) -> Result<Self, getrandom::Error> {
        let mut key = Zeroizing::new([0; 32]);
        getrandom::fill(&mut *key)?;
        Ok(Self::from_key(key, known))
    }

    #[cfg(test)]
    pub(super) fn with_key(key: [u8; 32], known: Vec<String>) -> Self {
        Self::from_key(Zeroizing::new(key), known)
    }

    fn from_key(key: Zeroizing<[u8; 32]>, known: Vec<String>) -> Self {
        Self {
            key,
            known: known.into_iter().map(Zeroizing::new).collect(),
            mapping: BTreeMap::new(),
            counts: Counts::default(),
        }
    }

    pub fn counts(&self) -> Counts {
        self.counts.clone()
    }

    pub fn add_known(&mut self, secret: &str) {
        if !secret.is_empty() && !self.known.iter().any(|known| known.as_str() == secret) {
            self.known.push(Zeroizing::new(secret.to_string()));
        }
    }

    fn candidate(&self, secret: &str, nonce: u64) -> String {
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(&self.key[..])
            .expect("HMAC accepts a 256-bit key");
        mac.update(&(secret.len() as u64).to_be_bytes());
        mac.update(secret.as_bytes());
        mac.update(&nonce.to_be_bytes());
        let mut digest = mac.finalize().into_bytes();
        let mut token = String::with_capacity(TOKEN_LENGTH);
        token.push_str(PREFIX);
        for byte in &digest[..6] {
            write!(&mut token, "{byte:02x}").expect("String writes cannot fail");
        }
        digest[..].zeroize();
        token
    }

    fn token(&mut self, secret: &str) -> String {
        let mut nonce = 0_u64;
        loop {
            let candidate = self.candidate(secret, nonce);
            match self.mapping.get(&candidate) {
                Some(previous) if previous.as_str() == secret => return candidate,
                Some(_) => nonce = nonce.checked_add(1).expect("token namespace exhausted"),
                None => {
                    self.mapping
                        .insert(candidate.clone(), Zeroizing::new(secret.to_string()));
                    return candidate;
                }
            }
        }
    }

    #[cfg(test)]
    pub(super) fn reserve_collision(&mut self, secret: &str) -> String {
        let candidate = self.candidate(secret, 0);
        self.mapping.insert(
            candidate.clone(),
            Zeroizing::new("DIFFERENT_FAKE_SECRET".to_string()),
        );
        candidate
    }

    pub fn redact_text<'a>(&mut self, text: &'a str) -> Cow<'a, str> {
        let found = detect::detect(text, self.known.iter().map(|key| key.as_str()));
        if found.is_empty() {
            return Cow::Borrowed(text);
        }
        let mut output = String::with_capacity(text.len());
        let mut consumed = 0;
        for matched in found {
            output.push_str(&text[consumed..matched.start]);
            output.push_str(&self.token(&text[matched.start..matched.end]));
            let count = self
                .counts
                .kinds
                .entry(matched.kind.to_string())
                .or_default();
            *count = count.saturating_add(1);
            consumed = matched.end;
        }
        output.push_str(&text[consumed..]);
        Cow::Owned(output)
    }

    pub fn redact_body<'a>(&mut self, body: &'a [u8]) -> Result<Cow<'a, [u8]>, serde_json::Error> {
        rewrite_body(body, &mut |text| match self.redact_text(text) {
            Cow::Borrowed(_) => None,
            Cow::Owned(text) => Some(text),
        })
    }

    pub fn restore_body<'a>(&mut self, body: &'a [u8]) -> Result<Cow<'a, [u8]>, serde_json::Error> {
        rewrite_body(body, &mut |text| {
            let restored = self.restore_text(text);
            (restored != text).then_some(restored)
        })
    }

    pub fn restore_text(&mut self, text: &str) -> String {
        self.restore(text, false)
    }

    fn restore(&mut self, text: &str, json: bool) -> String {
        static TOKENS: OnceLock<regex::Regex> = OnceLock::new();
        let tokens = TOKENS.get_or_init(|| {
            regex::Regex::new(r"clud_secret_[0-9a-f]{12}").expect("static token pattern")
        });
        let mut output = String::with_capacity(text.len());
        let mut consumed = 0;
        for token in tokens.find_iter(text) {
            output.push_str(&text[consumed..token.start()]);
            if let Some(secret) = self.mapping.get(token.as_str()) {
                if json {
                    let quoted = Zeroizing::new(
                        serde_json::to_string(secret.as_str())
                            .expect("credential strings serialize"),
                    );
                    output.push_str(&quoted[1..quoted.len() - 1]);
                } else {
                    output.push_str(secret);
                }
            } else {
                output.push_str(token.as_str());
                self.counts.unknown_tokens = self.counts.unknown_tokens.saturating_add(1);
            }
            consumed = token.end();
        }
        output.push_str(&text[consumed..]);
        output
    }

    pub fn restore_fragment(
        &mut self,
        pending: &mut String,
        text: &str,
        json: bool,
        finish: bool,
    ) -> String {
        let mut joined = std::mem::take(pending);
        joined.push_str(text);
        let split = if finish {
            joined.len()
        } else {
            partial_start(&joined)
        };
        pending.push_str(&joined[split..]);
        joined.truncate(split);
        self.restore(&joined, json)
    }
}

fn partial_start(text: &str) -> usize {
    let bytes = text.as_bytes();
    for length in (1..TOKEN_LENGTH)
        .rev()
        .filter(|length| *length <= bytes.len())
    {
        let suffix = &bytes[bytes.len() - length..];
        let partial = if length <= PREFIX.len() {
            PREFIX.as_bytes().starts_with(suffix)
        } else {
            suffix.starts_with(PREFIX.as_bytes())
                && suffix[PREFIX.len()..]
                    .iter()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
        };
        if partial {
            return bytes.len() - length;
        }
    }
    bytes.len()
}

fn rewrite_body<'a>(
    body: &'a [u8],
    transform: &mut impl FnMut(&str) -> Option<String>,
) -> Result<Cow<'a, [u8]>, serde_json::Error> {
    let mut value: serde_json::Value = serde_json::from_slice(body)?;
    if rewrite_value(&mut value, true, transform)? {
        serde_json::to_vec(&value).map(Cow::Owned)
    } else {
        Ok(Cow::Borrowed(body))
    }
}

fn rewrite_value(
    value: &mut serde_json::Value,
    skip_binary: bool,
    transform: &mut impl FnMut(&str) -> Option<String>,
) -> Result<bool, serde_json::Error> {
    match value {
        serde_json::Value::String(text) => {
            if let Some(replaced) = transform(text) {
                *text = replaced;
                Ok(true)
            } else {
                Ok(false)
            }
        }
        serde_json::Value::Array(values) => values.iter_mut().try_fold(false, |changed, value| {
            Ok(rewrite_value(value, skip_binary, transform)? | changed)
        }),
        serde_json::Value::Object(object) => rewrite_object(object, skip_binary, transform),
        _ => Ok(false),
    }
}

fn rewrite_object(
    object: &mut serde_json::Map<String, serde_json::Value>,
    skip_binary: bool,
    transform: &mut impl FnMut(&str) -> Option<String>,
) -> Result<bool, serde_json::Error> {
    let kind = object
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if skip_binary && matches!(kind, "image" | "input_image" | "output_image" | "image_url") {
        return Ok(false);
    }
    let tool_input = kind == "tool_use";
    let document = kind == "document";
    let mut changed = false;
    for (key, mut value) in std::mem::take(object) {
        let binary_source = skip_binary
            && document
            && key == "source"
            && value.get("type").and_then(serde_json::Value::as_str) == Some("base64");
        if !binary_source {
            changed |= rewrite_value(
                &mut value,
                skip_binary && !(tool_input && key == "input"),
                transform,
            )?;
        }
        let rewritten = transform(&key);
        changed |= rewritten.is_some();
        let key = rewritten.unwrap_or(key);
        if object.insert(key, value).is_some() {
            return Err(<serde_json::Error as serde::ser::Error>::custom(
                "secret transformation would merge JSON object keys",
            ));
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod key_tests {
    use super::*;

    #[test]
    fn tool_input_object_keys_are_tokenized_and_restored() {
        let secret = "FAKE_VAULT_KEY_USED_AS_JSON_MEMBER_NAME";
        let mut session = Session::with_key([8; 32], vec![secret.into()]);
        let body = serde_json::json!({"type":"tool_use","input":{secret:{"nested":secret}}});
        assert!(body["input"].get(secret).is_some());
        let bytes = serde_json::to_vec(&body).unwrap();
        let redacted = session.redact_body(&bytes).unwrap().into_owned();
        assert!(!String::from_utf8_lossy(&redacted).contains(secret));
        let restored = session.restore_body(&redacted).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&restored).unwrap(),
            body
        );
    }

    /// Providers cache the rendered prompt prefix, and tool schemas and
    /// `tool_use.input` objects are rendered as JSON text. Redacting one
    /// secret late in a conversation must therefore not reorder the keys of
    /// everything before it: that turn would re-send the whole history
    /// uncached (#2225).
    #[test]
    fn redaction_rewrites_only_the_secret_and_keeps_key_order() {
        let secret = "FAKE_VAULT_KEY_KEY_ORDER_FIXTURE";
        let mut session = Session::with_key([7; 32], vec![secret.into()]);
        let body = format!(
            concat!(
                r#"{{"model":"m","tools":[{{"name":"Bash","input_schema":"#,
                r#"{{"type":"object","properties":{{"command":{{"type":"string"}},"#,
                r#""description":{{"type":"string"}}}},"required":["command"]}}}}],"#,
                r#""messages":[{{"role":"assistant","content":[{{"type":"tool_use","#,
                r#""id":"t1","name":"Bash","input":{{"command":"ls","description":"d"}}}}]}},"#,
                r#"{{"role":"user","content":"{}"}}]}}"#
            ),
            secret
        );
        let redacted = session.redact_body(body.as_bytes()).unwrap().into_owned();
        let token = session.redact_text(secret).into_owned();
        assert_eq!(
            String::from_utf8(redacted).unwrap(),
            body.replace(secret, &token)
        );
    }

    #[test]
    fn object_key_collisions_fail_closed_in_both_directions() {
        let secret = "FAKE_VAULT_KEY_COLLISION_FIXTURE";
        let mut session = Session::with_key([9; 32], vec![secret.into()]);
        let token = session.redact_text(secret).into_owned();
        let body = serde_json::json!({"type":"tool_use","input":{secret:1,token:2}});
        let bytes = serde_json::to_vec(&body).unwrap();
        assert!(session.redact_body(&bytes).is_err());
        assert!(session.restore_body(&bytes).is_err());
    }
}

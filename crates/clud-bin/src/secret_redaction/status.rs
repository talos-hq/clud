//! Publish non-sensitive counters when a gateway request finishes.

use std::sync::Arc;

use crate::toast::statusline::StatusStateWriter;

use super::stream::SharedSession;

pub struct Report {
    session: Option<SharedSession>,
    writer: Option<Arc<StatusStateWriter>>,
}

impl Report {
    pub fn new(session: Option<&SharedSession>, writer: Option<Arc<StatusStateWriter>>) -> Self {
        Self {
            session: session.cloned(),
            writer,
        }
    }
}

impl Drop for Report {
    fn drop(&mut self) {
        let (Some(session), Some(writer)) = (&self.session, &self.writer) else {
            return;
        };
        let counts = session.lock().ok().map(|session| session.counts());
        if let Some(counts) =
            counts.filter(|counts| !counts.kinds.is_empty() || counts.unknown_tokens != 0)
        {
            writer.publish_redaction(counts);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret_redaction::reversible::Session;
    use std::sync::Mutex;

    #[test]
    fn request_completion_publishes_counts_without_tokens_or_values() {
        let dir = kernal_api::platform::fs::TemporaryDirectory::new().unwrap();
        let path = dir.path().join("status.json");
        let writer = Arc::new(StatusStateWriter::new(path.clone()));
        let secret = "FAKE_VAULT_CREDENTIAL_ONLY_FOR_STATUS_TEST";
        let session = Arc::new(Mutex::new(Session::with_key([4; 32], vec![secret.into()])));
        let token;
        {
            let _report = Report::new(Some(&session), Some(Arc::clone(&writer)));
            let mut session = session.lock().unwrap();
            token = session.redact_text(secret).into_owned();
            session.restore_text("clud_secret_deadbeef0000");
        }
        let serialized = std::fs::read_to_string(path).unwrap();
        assert!(!serialized.contains(secret));
        assert!(!serialized.contains(&token));
        let value: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(value["redaction"]["kinds"]["vault"], 1);
        assert_eq!(value["redaction"]["unknown_tokens"], 1);
    }
}

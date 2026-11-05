//! Structured installer outcomes; commit and activation are separate facts.

use std::io::{self, Write};
use std::path::Path;

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Status {
    Complete,
    ActivationRequired,
    Refused,
    Failed,
    BrowserOpened,
}

#[derive(Debug, Serialize)]
pub(super) struct Outcome {
    schema: &'static str,
    status: Status,
    exit_code: i32,
    version: Option<String>,
    destination: Option<String>,
    binary_committed: bool,
    activation_verified: bool,
    reason: Option<&'static str>,
}

impl Outcome {
    fn new(status: Status, exit_code: i32, reason: Option<&'static str>) -> Self {
        Self {
            schema: "clud-installer-result/v1",
            status,
            exit_code,
            version: None,
            destination: None,
            binary_committed: status == Status::Complete,
            activation_verified: status == Status::Complete,
            reason,
        }
    }

    pub(super) fn complete() -> Self {
        Self::new(Status::Complete, 0, None)
    }

    pub(super) fn activation_required() -> Self {
        Self::new(
            Status::ActivationRequired,
            0,
            Some("path_modification_disabled"),
        )
    }

    pub(super) fn refused(code: i32, reason: &'static str) -> Self {
        Self::new(Status::Refused, code, Some(reason))
    }

    pub(super) fn failed(code: i32, reason: &'static str) -> Self {
        Self::new(Status::Failed, code, Some(reason))
    }

    pub(super) fn browser_opened() -> Self {
        Self::new(Status::BrowserOpened, 0, None)
    }

    pub(super) fn selected(mut self, version: &str, destination: &Path) -> Self {
        self.version = Some(version.to_owned());
        self.destination = destination.to_str().map(str::to_owned);
        self
    }

    pub(super) fn committed(mut self, version: &str, destination: &Path) -> Self {
        self = self.selected(version, destination);
        self.binary_committed = true;
        self
    }

    pub(super) fn exit_code(&self) -> i32 {
        self.exit_code
    }

    pub(super) fn is_complete(&self) -> bool {
        self.status == Status::Complete
    }

    pub(super) fn write_json(&self, mut writer: impl Write) -> io::Result<()> {
        serde_json::to_writer(&mut writer, self).map_err(io::Error::other)?;
        writer.write_all(b"\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcome_json_keeps_partial_installation_distinct_from_complete() {
        for (outcome, expected) in [
            (Outcome::complete(), "complete"),
            (Outcome::activation_required(), "activation_required"),
            (Outcome::refused(1, "repair_declined"), "refused"),
            (Outcome::failed(1, "activation_failed"), "failed"),
        ] {
            let outcome = outcome.committed("2.8.31", Path::new("selected-clud"));
            let mut bytes = Vec::new();
            outcome.write_json(&mut bytes).unwrap();
            let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value["status"], expected);
            assert_eq!(value["binary_committed"], true);
            assert_eq!(value["activation_verified"], expected == "complete");
            assert_eq!(value["version"], "2.8.31");
            assert_eq!(bytes.iter().filter(|byte| **byte == b'\n').count(), 1);
        }
    }
}

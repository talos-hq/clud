//! Which entry point created a durable session.
//!
//! One typed field on the session record, set once at creation by the entry
//! point and never inferred later from argv or environment. Records written
//! before the field existed load as [`SessionOrigin::Unknown`].
//! See `docs/architecture/session-origin.md`.

use serde::{Deserialize, Serialize};

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum SessionOrigin {
    /// Created by a `clud` command-line client.
    Cli,
    /// Created by the `clud` MCP server (`session_start`).
    Mcp,
    /// Created by the daemon itself.
    Daemon,
    /// Created by a test harness.
    Test,
    /// Written before origins were recorded, or a client that did not say.
    #[default]
    Unknown,
}

impl SessionOrigin {
    /// Every variant, for table-driven tests and help text.
    pub const ALL: [Self; 5] = [
        Self::Cli,
        Self::Mcp,
        Self::Daemon,
        Self::Test,
        Self::Unknown,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Mcp => "mcp",
            Self::Daemon => "daemon",
            Self::Test => "test",
            Self::Unknown => "unknown",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::ValueEnum;

    #[test]
    fn serde_clap_and_as_str_agree_for_every_variant() {
        for origin in SessionOrigin::ALL {
            let json = serde_json::to_value(origin).unwrap();
            assert_eq!(json, origin.as_str());
            assert_eq!(
                serde_json::from_value::<SessionOrigin>(json).unwrap(),
                origin
            );
            assert_eq!(SessionOrigin::from_str(origin.as_str(), false), Ok(origin));
        }
        assert_eq!(
            SessionOrigin::value_variants().len(),
            SessionOrigin::ALL.len()
        );
    }
}

//! Bound concurrent daemon session launches, including those still starting.

use std::io;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::session_registry::{SessionRegistry, CAP_DISABLED};

static SESSIONS: AtomicU64 = AtomicU64::new(0);

pub(super) struct SessionAdmission;

impl SessionAdmission {
    pub(super) fn acquire() -> io::Result<Self> {
        let limit = SessionRegistry::cap_config_from_env().max;
        SESSIONS
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                (limit == CAP_DISABLED || count < limit).then(|| count.saturating_add(1))
            })
            .map_err(|_| io::Error::other(format!("daemon session limit reached ({limit})")))?;
        Ok(Self)
    }
}

impl Drop for SessionAdmission {
    fn drop(&mut self) {
        SESSIONS.fetch_sub(1, Ordering::Relaxed);
    }
}

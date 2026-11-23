//! The verified subscription wire identity and its refresh budget.

use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use crate::server_settings::{self, CodexCompatibility};

const REFRESH_COOLDOWN: Duration = Duration::from_secs(60);

#[derive(Clone, Copy)]
pub(super) enum Lookup {
    Current,
    Refresh,
}

struct Compatibility {
    version: Mutex<String>,
    last_refresh: Mutex<Option<Instant>>,
}

impl Compatibility {
    fn new(version: String) -> Self {
        Self {
            version: Mutex::new(version),
            last_refresh: Mutex::new(None),
        }
    }

    fn resolve(&self, lookup: Lookup, fetch: impl FnOnce() -> Option<String>) -> String {
        if matches!(lookup, Lookup::Refresh) {
            // Recovery requests share one refresh. Ordinary lookups can still
            // use the current value while the bounded network request runs.
            let mut last_refresh = lock(&self.last_refresh);
            if last_refresh.is_none_or(|at| at.elapsed() >= REFRESH_COOLDOWN) {
                *last_refresh = Some(Instant::now());
                if let Some(version) = fetch() {
                    *lock(&self.version) = version;
                }
            }
        }
        lock(&self.version).clone()
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

pub(super) fn subscription_client_version(lookup: Lookup) -> String {
    static STATE: OnceLock<Compatibility> = OnceLock::new();
    let state = STATE.get_or_init(|| {
        Compatibility::new(
            server_settings::snapshot()
                .section::<CodexCompatibility>()
                .client_version,
        )
    });
    state.resolve(lookup, || {
        server_settings::refresh_section::<CodexCompatibility>().map(|settings| {
            server_settings::trace(format_args!(
                "refreshed Codex subscription client version to {}",
                settings.client_version
            ));
            settings.client_version
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_replaces_current_identity_and_is_coalesced() {
        let state = Compatibility::new("0.164.0".to_string());
        assert_eq!(state.resolve(Lookup::Current, || unreachable!()), "0.164.0");
        assert_eq!(
            state.resolve(Lookup::Refresh, || Some("0.171.0".to_string())),
            "0.171.0"
        );
        assert_eq!(state.resolve(Lookup::Refresh, || unreachable!()), "0.171.0");
        assert_eq!(state.resolve(Lookup::Current, || unreachable!()), "0.171.0");
    }

    #[test]
    fn unsuccessful_refresh_keeps_the_last_verified_identity_and_cools_down() {
        let state = Compatibility::new("0.171.0".to_string());
        assert_eq!(state.resolve(Lookup::Refresh, || None), "0.171.0");
        assert_eq!(state.resolve(Lookup::Refresh, || unreachable!()), "0.171.0");
    }
}

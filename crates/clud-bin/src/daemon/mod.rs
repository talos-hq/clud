mod activity;
mod api_session_http;
pub mod api_session_lifecycle;
pub mod api_sessions;
pub mod api_turn_controller;
mod attach;
mod attach_input;
mod board;
mod checkout_claims;
mod client;
mod client_compat;
mod client_leases;
mod commands;
mod cpu_alert_publish;
mod daemon_events;
mod entry;
mod gc_service;
mod handover_registry;
mod headless_adapter;
mod http;
pub(crate) mod io_helpers;
mod login_env;
mod paths;
mod proc_sampler;
mod process_utils;
mod registry_holder;
mod rp_broker;
mod runtime_config;
mod server;
mod session_flags;
mod session_state_sweep;
mod session_tmp_sweep;
mod sessions;
mod startup_phases;
mod target_sweep;
mod top;
mod trash_cap;
mod types;
pub mod uv_cache_sweep;
mod watch_service;
mod wire_prost;
mod worker;
mod worker_shared;

pub(crate) use checkout_claims::claimed_by_other;
pub use checkout_claims::{
    foreground_session_id, run_claim_command, start_foreground_presence, ForegroundCheckoutPresence,
};
pub use client::try_register_gc_watch;
pub use client::{
    acquire_foreground_client_lease, daemon_client_metrics, ensure_daemon, gc_client_insert,
    gc_client_insert_created, gc_client_list, gc_client_list_repo_visits, gc_client_purge,
    gc_client_query_created, gc_client_reconcile, gc_client_record_repo_visit,
    is_incompatible_daemon_error, print_incompatible_daemon_error, try_handoff_kill_to_daemon,
    try_request_orphan_reap, ForegroundClientLease, GcPurgeOutcome, LEDGER_QUERY_TIMEOUT,
};
pub use entry::{experimental_enabled, handle_special_command, run_centralized_session};
/// Issue #1606: `--clean-worktrees` shares the daemon's repo-worktree
/// verdict, probe and reclaim executor through this one bridge.
pub(crate) use gc_service::repo_worktree_cli;
pub use http::{
    dashboard_url_from_info, fetch_state_json, read_api_info, read_dashboard_info,
    read_dashboard_port, DashboardInfo,
};
pub use session_flags::{session_flag_get, session_flag_set, session_loop_event};
/// `types` is private; re-export just the state-dir env-var name so
/// `gc::delete_audit` (#893) resolves the same variable without a second
/// definition of the string.
pub use types::ENV_STATE_DIR;
// Issue #469: re-exports for the telemetry integration test under
// `tests/integration/diagnostics/telemetry_endpoint.rs` which spawns the dashboard server
// directly and asserts the full HTTP round-trip.
#[cfg(test)]
pub use cpu_alert_publish::SAMPLE_INTERVAL as CPU_SAMPLE_INTERVAL_FOR_TEST;
pub use cpu_alert_publish::{metrics_snapshot_path, MetricsSnapshot};
pub use daemon_events::log_event as log_structured_event;
pub use http::{
    spawn_dashboard_telemetry_only, DashboardState, TelemetryEntry, TelemetryIngest,
    TelemetryPidDetail, TelemetryPidSummary, TelemetryStore,
};
pub use paths::{default_state_dir, default_trash_dir};
pub use types::GcWatchRoot;
pub use types::{ListRow, RepoVisit, ENV_ALLOW_DAEMON_SPAWN};

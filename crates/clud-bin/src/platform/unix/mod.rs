//! Unix (Linux and macOS) implementations of the `crate::platform` areas.

pub mod alias_link;
pub mod attach_input;
pub mod backend_bootstrap;
pub mod block_bad_cmd_io;
pub mod block_bad_cmd_repeat;
#[cfg(test)]
mod build_id;
pub mod codex_auth;
pub mod config;
pub mod console_input;
pub mod console_setup;
pub mod console_title;
pub mod crash_report;
pub mod credential_file;
pub mod ctrl_c_track;
pub mod dnd;
pub mod file_identity;
pub mod fs_private;
#[cfg(test)]
pub mod gc_test_fs;
#[cfg(test)]
mod gh_broker;
pub mod grind_scripts;
pub mod host;
pub mod job_orphan_reaper;
#[cfg(test)]
pub mod large_file_guard;
pub mod log_event;
pub mod login_env;
#[cfg(test)]
mod nounset;
pub mod optimize;
pub mod path_norm;
pub mod process_identity;
#[cfg(test)]
mod process_identity_zombie_tests;
#[cfg(test)]
mod process_scan;
pub mod process_tree;
pub mod provider_auth;
pub mod reaper_facts;
pub mod repo_worktree_reclaim_exec;
pub mod rm_guard;
pub mod rm_tool;
pub mod runner;
pub mod runner_execution;
pub mod runtime_cache;
pub mod safe_mktemp;
pub mod secret_helper;
pub mod secret_prompt;
pub mod self_install;
pub mod self_install_activation;
pub mod session;
pub mod session_registry;
pub mod session_tmp_continuation;
pub mod shim_fs;
pub mod shim_main;
pub mod shim_telemetry;
pub mod start_terminal;
pub mod startup;
pub mod stdio;
pub mod subprocess;
pub mod test_capture;
pub mod trampoline;
pub mod usage_ledger;
pub mod video;
pub mod voice;
pub mod wedge_watchdog;
#[cfg(test)]
mod win_creation_flags_tests;
pub mod worker_shared;

//! Library surface for the `clud` binary crate. Exposed so integration tests
//! under `tests/` can link against internals (notably `session::run_raw_pty_pump`
//! and `session::F3Observer`). The production binary in `src/main.rs` imports
//! modules from this library rather than declaring its own `mod ...` copies,
//! so there is exactly one instance of each module in the build.

// Both registries expose the same redb conversion set.
macro_rules! impl_from_redb {
    ($error:ident) => {
        impl_from_redb!(@one $error; redb::Error);
        impl_from_redb!(@one $error; redb::DatabaseError);
        impl_from_redb!(@one $error; redb::TransactionError);
        impl_from_redb!(@one $error; redb::TableError);
        impl_from_redb!(@one $error; redb::StorageError);
        impl_from_redb!(@one $error; redb::CommitError);
    };
    (@one $error:ident; $redb_error:ty) => {
        impl From<$redb_error> for $error {
            fn from(error: $redb_error) -> Self {
                Self::Sql(error.to_string())
            }
        }
    };
}

pub mod alias_link;
pub mod anthropic_usage;
pub mod args;
pub mod attribution;
pub mod auth;
pub mod backend;
pub mod backend_bootstrap;
pub mod block_bad_cmd;
pub mod block_bad_cmd_rollout;
pub mod bridge_log;
pub mod cache_health;
pub mod cache_heartbeat;
pub mod capture;
pub mod ci_local;
pub mod civil_time;
pub mod claude_files;
pub mod claude_worktree_hook;
pub mod clud_hook_roots;
pub mod clud_hooks;
pub mod clud_hooks_compile;
pub mod clud_hooks_run;
pub mod clud_settings;
pub mod codex_auth;
pub mod codex_bridge;
pub mod codex_cmd_scan_install;
pub mod codex_history;
pub mod codex_hook_normalize;
pub mod codex_lf;
pub mod codex_model;
pub mod codex_pipeline;
pub mod codex_runtime;
pub mod codex_sse;
pub mod codex_translate;
pub mod codex_upstream;
pub mod command;
pub mod config;
/// The Windows native terminal-input adapter (`platform/windows/console_input.rs`);
/// an empty module on Unix, where stdin is a byte stream.
pub use platform::console_input;
pub mod console_setup;
pub mod console_surrogates;
pub mod console_title;
pub mod cpu_banner;
pub mod crash_report;
pub mod credential_chain;
pub mod ctrl_c_track;
pub mod daemon;
pub mod dashboard_auth;
pub mod doctor;
pub mod dsh_harness;
pub mod extern_cli;
pub mod extern_root;

pub mod build_id;
pub mod clud_repo_dev;
pub mod dnd;
pub mod failover;
pub mod foreground_runtime;
pub mod fs_private;
pub mod gate_replay;
pub mod gc;
pub mod gemini_client_ingress;
pub mod gh_broker;
pub mod graphics;
pub mod grind;
pub mod grind_facts;
pub mod grind_reconcile;
pub mod harness_picker;
pub mod home;
pub mod hook_handshake;
pub mod hook_health;
pub mod hook_trust;
pub mod job_orphan_reaper;
pub mod large_file_guard;
pub mod launch_context;
pub mod launch_log;
pub mod launch_setup;
pub mod log_event;
pub mod loop_artifacts;
pub mod loop_check;
pub mod loop_plan_mode;
pub mod loop_spec;
pub mod multicall;
pub mod openai_ingress;
pub mod openrouter_catalog;
pub mod openrouter_free;
pub mod openrouter_routing;
pub mod optimize;
pub mod orphan_reaper;
pub mod paste_image;
pub mod path_norm;
pub mod platform;
pub mod preference;
pub mod process_identity;
pub mod process_scan;
pub mod process_tree;
pub mod provider_auth;
pub mod provider_catalog;
pub mod provider_registry;
pub mod reap_log;
pub mod reaper_facts;
pub mod repo_clud_config;
pub mod route_health;
pub mod route_plan;
pub mod runner;
pub mod runtime_cache;
pub mod secret_prompt;
pub mod secret_redaction;
pub mod selector;
pub mod self_install;
pub mod serve_gateway;
mod serve_transport;
pub mod server_settings;
pub mod session;
pub mod session_history;
pub mod session_index;
pub mod session_registry;
pub mod settings_tui;
pub mod shell;
pub mod shell_ast;
pub mod shim_install;
pub mod shim_main;
pub mod shim_registry;
pub mod shim_resolve;
pub mod shim_session;
pub mod shim_telemetry;
pub mod shim_uv;
pub mod skills;
pub mod soldr_activate;
pub mod stage_trace;
pub mod startup;
pub mod stop_repeat;
pub mod stream_json;
pub mod subprocess;
#[cfg(test)]
pub(crate) mod test_env;
pub mod video;

pub mod kitty_term;
pub mod symbols;
pub(crate) mod terminal_queries;
pub mod test_runtime;
pub mod toast;
pub mod tool_cli;
pub mod tool_info;
pub mod tool_install;
pub mod tool_ledger;
pub mod tool_list;
pub mod tool_log;
pub mod tool_query;
pub mod tool_run;
pub mod tool_tee;
pub mod tool_termination;
pub mod tool_watchdog;
pub mod tools;
pub mod trampoline;
pub mod trash;
pub mod ui;
pub mod uv_run_hook_guard;
pub mod verbose_log;
pub mod voice;
pub mod wasm;
pub mod webterm;
pub mod wedge_watchdog;
pub mod win_creation_flags;
pub mod workspace_trust;
pub mod worktree_consent;
pub mod worktrees;

pub mod deletion_policy;
pub mod deletion_rules;
pub mod rm_guard;
pub mod rm_tool;
pub mod safe_mktemp;

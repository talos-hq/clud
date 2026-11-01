mod builder;
mod do_input;
pub mod do_kind;
pub mod do_prompt;
pub mod goal_compact;
pub mod grind_scripts;
mod loop_task;
mod mcp_config;
mod prompts;
mod types;

pub(crate) use builder::build_headless_turn_plan;
#[allow(unused_imports)]
pub(crate) use builder::parse_repeat_interval;
pub use builder::{
    bridge_suppresses_plan_mode, build_launch_plan, build_launch_plan_for_target,
    grind_launch_error, has_noninteractive_prompt, interactive_builtin_resume_error,
    next_run_at_millis, plan_mode_suppression_notice, repeat_implies_no_done_warning,
    summarize_task_name, video_launch_error,
};
pub use do_input::resolve_do_command_target;
pub(crate) use types::{HeadlessSession, HeadlessTurnRequest};
pub use types::{LaunchPlan, LoopMarkers, RepeatSchedule};

#[cfg(test)]
mod tests;

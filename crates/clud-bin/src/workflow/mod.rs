//! `clud workflow`: run Anthropic-format workflow scripts (#2471).
//!
//! A script starts with `export const meta = {name, description, phases}`
//! and then uses `agent()`, `parallel()`, `pipeline()`, `phase()` and the
//! global `args`, with top-level `await` and `return`. The source is wrapped
//! in an async function and evaluated by an embedded QuickJS engine.
//!
//! `agent()` never blocks inside JS. It returns a pending promise and queues
//! an [`AgentRequest`]. The host loop drains QuickJS jobs, hands every
//! queued request to the runner as one batch (the runner may execute them
//! concurrently), resolves the promises with the results, and repeats until
//! the script's promise settles. No async runtime is required.

pub mod cache;
pub mod cli;
mod engine;
mod meta;
pub mod runner;

pub use engine::{run_script, AgentBatchRunner, AgentRequest, WorkflowOutcome};
pub use meta::{Meta, MetaError, Phase};

#[cfg(test)]
mod tests;

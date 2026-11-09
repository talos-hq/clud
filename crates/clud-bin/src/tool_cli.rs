//! Shared dispatcher for `clud tool ...` and the `clud tool` alias.

use crate::{args::ToolSubcommand, tool_info, tool_ledger, tool_list, tool_log, tool_run};

pub fn run(subcommand: &ToolSubcommand) -> i32 {
    match subcommand {
        ToolSubcommand::Run { rel_path, args } => match tool_run::run(rel_path, args) {
            Ok(code) => code,
            Err(err) => {
                eprintln!("[clud tool] run failed: {err}");
                2
            }
        },
        ToolSubcommand::List { json, long } => match tool_list::run(*json, *long) {
            Ok(code) => code,
            Err(err) => {
                eprintln!("[clud tool] list failed: {err}");
                2
            }
        },
        ToolSubcommand::Info(options) => {
            let crate::args::ToolInfoArgs {
                reference,
                pid,
                lines,
                json,
            } = options.as_ref();
            match tool_info::run(reference.as_deref(), *pid, *lines, *json) {
                Ok(code) => code,
                Err(err) => {
                    eprintln!("[clud tool] info failed: {err}");
                    2
                }
            }
        }
        ToolSubcommand::Log(options) => {
            let crate::args::ToolLogArgs {
                reference,
                pid,
                stream,
                since,
                until,
                between,
                grep,
                head,
                tail,
                json,
            } = options.as_ref();
            let Some(stream_sel) = tool_log::StreamSelector::parse(stream) else {
                eprintln!("[clud tool] log: --stream must be stdout|stderr|combined");
                return 2;
            };
            let between_pair = between.as_ref().and_then(|v| {
                if v.len() == 2 {
                    Some((v[0].as_str(), v[1].as_str()))
                } else {
                    None
                }
            });
            match tool_log::run(
                reference.as_deref(),
                *pid,
                stream_sel,
                since.as_deref(),
                until.as_deref(),
                between_pair,
                grep.as_deref(),
                *head,
                *tail,
                *json,
            ) {
                Ok(code) => code,
                Err(err) => {
                    eprintln!("[clud tool] log failed: {err}");
                    2
                }
            }
        }
        ToolSubcommand::Ledger(options) => {
            let crate::args::ToolLedgerArgs {
                tool,
                session,
                json,
            } = options.as_ref();
            let Some(scope) = tool_ledger::SessionScope::parse(session) else {
                eprintln!("[clud tool] ledger: --session must be current|previous|all");
                return 2;
            };
            match tool_ledger::run(tool.as_deref(), scope, *json) {
                Ok(code) => code,
                Err(err) => {
                    eprintln!("[clud tool] ledger failed: {err}");
                    2
                }
            }
        }
    }
}

//! Generator for `docs/cli-grammar.md`, the one reference for clud's CLI
//! grammar (every command, subcommand, flag and positional argument).
//!
//! The document is rendered from the real clap tree
//! ([`crate::args::parser_command`]), the passthrough splitter's tables in
//! `args.rs`, and the argv[0] dispatch table in [`crate::multicall`], so it
//! cannot describe a flag clud does not parse. Nothing here ships: the module
//! is compiled for tests only, and the drift test below fails while the
//! committed file differs from a fresh render. Regenerate with
//! `CLUD_REGEN_CLI_GRAMMAR=1 soldr cargo test -p clud --lib cli_grammar`.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fmt::Write as _;

use clap::{Arg, ArgAction, Command};

use crate::args;
use crate::multicall::{self, Personality};

/// Repository-relative path of the rendered document.
pub(crate) const DOC_PATH: &str = "docs/cli-grammar.md";
/// Set to rewrite the committed document instead of checking it.
const REGEN_ENV: &str = "CLUD_REGEN_CLI_GRAMMAR";
/// The command that regenerates the document, quoted in its header and in
/// the drift test's failure.
const REGEN_COMMAND: &str = "CLUD_REGEN_CLI_GRAMMAR=1 soldr cargo test -p clud --lib cli_grammar";

/// Render the whole document. Building clap's tree is one large debug-build
/// frame, so the render runs on the parser's own stack.
pub(crate) fn render() -> String {
    args::on_arg_parser_stack(render_on_parser_stack)
}

fn render_on_parser_stack() -> String {
    let mut root = args::parser_command();
    root.build();
    let commands = command_paths(&root);
    let mut out = String::new();
    out.push_str(&header());
    out.push_str(&pipeline_section());
    out.push_str(&ebnf_section(&commands));
    out.push_str(&passthrough_section());
    out.push_str(&multicall_section());
    out.push_str(&environment_section(&commands));
    out.push_str(&exit_code_section(&commands));
    out.push_str(&commands_section(&commands));
    mark_interpreter_names(&out)
}

/// One command in the tree with the words that reach it (`["clud", "gc", "prune"]`).
struct CommandPath<'a> {
    words: Vec<String>,
    command: &'a Command,
    hidden: bool,
}

impl CommandPath<'_> {
    fn display(&self) -> String {
        self.words.join(" ")
    }

    fn rule(&self) -> String {
        self.words.join("-").replace('.', "-")
    }
}

/// Every command in declaration order, depth first. clap's generated `help`
/// subcommands are omitted: they only print help.
fn command_paths(root: &Command) -> Vec<CommandPath<'_>> {
    let mut out = Vec::new();
    collect_paths(root, vec!["clud".to_string()], false, &mut out);
    out
}

fn collect_paths<'a>(
    command: &'a Command,
    words: Vec<String>,
    hidden: bool,
    out: &mut Vec<CommandPath<'a>>,
) {
    out.push(CommandPath {
        words: words.clone(),
        command,
        hidden,
    });
    for sub in visible_subcommands(command) {
        let mut next = words.clone();
        next.push(sub.get_name().to_string());
        collect_paths(sub, next, hidden || sub.is_hide_set(), out);
    }
}

fn visible_subcommands(command: &Command) -> impl Iterator<Item = &Command> {
    command
        .get_subcommands()
        .filter(|sub| sub.get_name() != "help")
}

fn is_help_or_version(arg: &Arg) -> bool {
    matches!(
        arg.get_action(),
        ArgAction::Help | ArgAction::HelpShort | ArgAction::HelpLong | ArgAction::Version
    )
}

fn options(command: &Command) -> Vec<&Arg> {
    command
        .get_arguments()
        .filter(|arg| !arg.is_positional() && !is_help_or_version(arg))
        .collect()
}

fn positionals(command: &Command) -> Vec<&Arg> {
    command.get_positionals().collect()
}

fn header() -> String {
    format!(
        "# clud CLI grammar\n\n\
         <!-- GENERATED FILE: do not edit. Source: crates/clud-bin/src/cli_grammar.rs.\n\
         Regenerate: {REGEN_COMMAND} -->\n\n\
         The complete command-line grammar of `clud`, rendered from the clap\n\
         definitions in `crates/clud-bin/src/args.rs` (and the subcommand\n\
         parsers it embeds), the passthrough splitter's tables, and the\n\
         argv[0] dispatch table in `crates/clud-bin/src/multicall.rs`. A drift\n\
         test fails CI when this file and the code disagree.\n\n\
         `clud --help` and `clud <command> --help` print the same facts for one\n\
         command. Why the launch pipeline is shaped this way lives in\n\
         [launch-plan.md](architecture/launch-plan.md) and\n\
         [provider-selection.md](architecture/provider-selection.md).\n\n\
         Contents: [argv pipeline](#argv-pipeline) · [grammar](#grammar-ebnf) ·\n\
         [passthrough](#passthrough-rules) · [multicall names](#multicall-names) ·\n\
         [environment](#environment-variables) · [exit codes](#exit-codes) ·\n\
         [commands](#commands)\n\n"
    )
}

fn pipeline_section() -> String {
    "## argv pipeline\n\n\
     Every invocation passes these stages in order (`main.rs`, `args.rs`):\n\n\
     1. **argv[0] dispatch.** If the executable was invoked under a helper name\n\
        (see [multicall names](#multicall-names)), that function runs and nothing\n\
        below applies.\n\
     2. **Normalization**, in the top-level option region only (before the first\n\
        subcommand or `--`): a numeric value after `-c`/`--continue` becomes the\n\
        session index (`clud -c 2`); a bare `--resume`/`-r` followed by a\n\
        subcommand or flag means \"open the picker\" (use `--resume=<session>` for\n\
        a session named like a verb); a flag glued to its value by a non-ASCII\n\
        space is split; a public long option spelled with a Unicode dash (or\n\
        `-deepseek`) is corrected to `--`; `--deepseek=<KEY>`, `--kimi=<KEY>` and\n\
        `--openrouter=<KEY>` split into flag and key, and any non-key value is an\n\
        error.\n\
     3. **Splitting** (`split_known_unknown`): tokens clud owns go to clap; every\n\
        other token is forwarded to the backend harness (see\n\
        [passthrough rules](#passthrough-rules)). A flag-shaped near miss of a\n\
        clud option is rejected with a suggestion instead of being forwarded.\n\
     4. **Placement check.** A root-only flag written after a subcommand\n\
        (`clud do --model x`) is rejected with the reordered command line.\n\
     5. **clap parse** of the owned tokens against the tables under\n\
        [commands](#commands). For `safe-rm`, every token after the verb is\n\
        handed to its rm-style parser verbatim.\n\n"
        .to_string()
}

fn ebnf_section(commands: &[CommandPath<'_>]) -> String {
    let mut out = String::from(
        "## Grammar (EBNF)\n\n\
         ISO 14977 EBNF: `,` concatenation, `|` alternation, `[ ]` optional,\n\
         `{ }` zero or more. `OPTION` is any flag in the enclosing command's\n\
         table under [commands](#commands), in any order and interleaved with\n\
         positionals; an upper-case name is one argv token. Hidden internal\n\
         commands are included because they parse.\n\n```ebnf\n",
    );
    for path in commands {
        out.push_str(&ebnf_rule(path));
    }
    out.push_str(
        "OPTION = ? a flag from the enclosing command's flag table ? ;\n\
         BACKEND-ARGS = ? any token; forwarded verbatim to the harness ? ;\n```\n\n",
    );
    out
}

fn ebnf_rule(path: &CommandPath<'_>) -> String {
    let command = path.command;
    let is_root = path.words.len() == 1;
    let mut terms = vec![ebnf_literal(command, is_root)];
    if !options(command).is_empty() {
        terms.push("{ OPTION }".to_string());
    }
    let (after_separator, ordinary): (Vec<&Arg>, Vec<&Arg>) = positionals(command)
        .into_iter()
        .partition(|arg| arg.is_last_set());
    terms.extend(ordinary.iter().map(|arg| ebnf_positional(arg)));
    let subs: Vec<String> = visible_subcommands(command)
        .map(|sub| format!("{}-{}", path.rule(), sub.get_name().replace('.', "-")))
        .collect();
    if !subs.is_empty() {
        let alternation = subs.join(" | ");
        terms.push(if command.is_subcommand_required_set() {
            format!("( {alternation} )")
        } else {
            format!("[ {alternation} ]")
        });
    }
    terms.extend(after_separator.iter().map(|arg| {
        let name = value_name(arg).replace('_', "-");
        format!("[ \"--\" , {{ {name} }} ]")
    }));
    format!("{} = {} ;\n", path.rule(), terms.join(" , "))
}

fn ebnf_literal(command: &Command, is_root: bool) -> String {
    let names: Vec<String> = std::iter::once(command.get_name())
        .chain(command.get_all_aliases())
        .map(|name| format!("\"{name}\""))
        .collect();
    if names.len() == 1 || is_root {
        names[0].clone()
    } else {
        format!("( {} )", names.join(" | "))
    }
}

fn value_name(arg: &Arg) -> String {
    arg.get_value_names()
        .and_then(|names| names.first())
        .map_or_else(|| arg.get_id().as_str().to_uppercase(), ToString::to_string)
}

fn repeats(arg: &Arg) -> bool {
    matches!(arg.get_action(), ArgAction::Append | ArgAction::Count)
        || arg
            .get_num_args()
            .is_some_and(|range| range.max_values() > 1)
}

fn ebnf_positional(arg: &Arg) -> String {
    let name = value_name(arg).replace(' ', "-");
    match (arg.is_required_set(), repeats(arg)) {
        (true, true) => format!("{name} , {{ {name} }}"),
        (true, false) => name,
        (false, true) => format!("{{ {name} }}"),
        (false, false) => format!("[ {name} ]"),
    }
}

fn code_list(items: &[&str]) -> String {
    items
        .iter()
        .map(|item| format!("`{item}`"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn passthrough_section() -> String {
    let mut out = String::from(
        "## Passthrough rules\n\n\
         clud is a launcher: an argv token it does not own is forwarded to the\n\
         backend harness (Claude Code, Codex, ...) unchanged, after clud's own\n\
         arguments. The splitter decides ownership token by token, left to right:\n\n\
         1. `--` ends clud's flags: everything after it is forwarded verbatim,\n\
            except inside a subcommand that owns `--` (below).\n\
         2. The first top-level command word switches to that command; from then\n\
            on every token belongs to the command's own parser.\n\
         3. A known switch, a known value flag plus its value, or a known\n\
            `--flag=value` stays with clud.\n\
         4. Any other token is forwarded. A flag-shaped token that misspells a\n\
            clud option, or names an option the splitter forgot, is an error.\n\n",
    );
    let lines: [(&str, &[&str]); 7] = [
        ("Top-level command words", args::TOP_LEVEL_SUBCOMMANDS),
        (
            "Commands that own `--` (their raw argv follows it)",
            args::SEPARATOR_OWNING_SUBCOMMANDS,
        ),
        (
            "Value flags (consume the next token)",
            args::SPLITTER_VALUE_FLAGS,
        ),
        ("Short value flags", args::SPLITTER_SHORT_VALUE_FLAGS),
        ("Switches", args::SPLITTER_BOOL_FLAGS),
        ("Short switches", args::SPLITTER_SHORT_BOOL_FLAGS),
        (
            "Switches with an optional `=`-joined value",
            args::OPTIONAL_VALUE_FLAGS,
        ),
    ];
    for (label, items) in lines {
        let _ = writeln!(out, "- **{label}:** {}", code_list(&dedup(items)));
    }
    let _ = writeln!(
        out,
        "- **Provider switches that accept an inline API key** (`--flag <KEY>` or \
         `--flag=<KEY>`, keys start with `sk-`): {}\n",
        code_list(args::INLINE_KEY_PROVIDER_FLAGS)
    );
    out
}

fn dedup<'a>(items: &[&'a str]) -> Vec<&'a str> {
    let mut seen = Vec::new();
    for item in items {
        if !seen.contains(item) {
            seen.push(*item);
        }
    }
    seen
}

fn personality_summary(personality: Personality) -> &'static str {
    match personality {
        Personality::CmdScan => "PreToolUse command scanner (`block_bad_cmd`)",
        Personality::Shim => "session shim: interpreter, `gh`, `git` and `rm` aliases",
        Personality::Secret => "masked credential prompt (`clud secret`)",
        Personality::Askpass => "`GIT_ASKPASS` / `SSH_ASKPASS` helper",
        Personality::RemoteShell => "remote-assistance PTY session leader",
        Personality::Installer => "native installer",
    }
}

/// Installer names are recognized by stem rather than registered as aliases.
const INSTALLER_NAMES: &[&str] = &["clud-installer", "clud-install", "clud-setup"];

fn multicall_section() -> String {
    let mut out = String::from(
        "## Multicall names\n\n\
         `clud` is the only shipped executable. Invoked under one of these\n\
         names (a hardlink, symlink or copy; case-, `.exe`- and\n\
         separator-insensitive), it runs that function before any parsing\n\
         above. Each name's own argv is that tool's, not clud's.\n\n\
         | argv[0] | Function |\n|---|---|\n",
    );
    let mut names = multicall::alias_names();
    names.extend(INSTALLER_NAMES.iter().map(ToString::to_string));
    for name in names {
        let personality = multicall::personality_for(OsStr::new(&name))
            .unwrap_or_else(|| panic!("multicall name {name} selects no personality"));
        let _ = writeln!(out, "| `{name}` | {} |", personality_summary(personality));
    }
    let _ = write!(
        out,
        "\nInstaller names also match with a browser download suffix\n\
         (`clud-installer (1)`). Three hidden argv[1] forms reach the same entry\n\
         points without an alias: `clud {} [ARGS]`, `clud {} <ALIAS> [ARGS]` and\n\
         `clud {} <DIR>`.\n\n",
        multicall::CMD_SCAN_SUBCOMMAND,
        multicall::SHIM_SUBCOMMAND,
        multicall::LINK_ALIASES_SUBCOMMAND
    );
    out
}

/// Words in help text that name an environment variable.
fn env_names(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
        .filter(|word| (word.starts_with("CLUD_") || word.starts_with("CLAUDE_")) && word.len() > 6)
        .map(str::to_string)
        .collect()
}

fn environment_section(commands: &[CommandPath<'_>]) -> String {
    let mut by_name: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for path in commands {
        let mut record = |text: String, at: String| {
            for name in env_names(&text) {
                let places = by_name.entry(name).or_default();
                if !places.contains(&at) {
                    places.push(at.clone());
                }
            }
        };
        record(about(path.command), format!("`{}`", path.display()));
        for arg in path.command.get_arguments() {
            record(
                long_help(arg),
                format!("`{}` {}", path.display(), arg_label(arg)),
            );
        }
    }
    let mut out = String::from(
        "## Environment variables\n\n\
         clap reads no environment variable: argv alone decides the parse. These\n\
         variables, named in the help of the listed command or flag, change what\n\
         a parsed command does.\n\n\
         | Variable | Named by |\n|---|---|\n",
    );
    for (name, places) in by_name {
        let _ = writeln!(out, "| `{name}` | {} |", places.join(", "));
    }
    out.push('\n');
    out
}

/// Sentences of a command's help that state an exit status.
fn exit_sentences(text: &str) -> Vec<String> {
    text.split_inclusive(". ")
        .map(|sentence| sentence.trim().to_string())
        .filter(|sentence| {
            let lower = sentence.to_lowercase();
            lower.contains("exit")
                && (lower.chars().any(|c| c.is_ascii_digit()) || lower.contains("non-zero"))
        })
        .collect()
}

fn exit_code_section(commands: &[CommandPath<'_>]) -> String {
    let mut out = String::from(
        "## Exit codes\n\n\
         - `0`: success, and `--help` / `--version`.\n\
         - `2`: a usage error (unknown subcommand, bad value, conflicting flags,\n\
           misplaced global flag, invalid inline key) reported before anything runs.\n\
         - A backend launch exits with the harness's own status; `clud test run`\n\
           returns its child's exit code unchanged.\n\n\
         Commands that document their own statuses:\n\n\
         | Command | Statement |\n|---|---|\n",
    );
    for path in commands {
        let text = long_about(path.command);
        for sentence in exit_sentences(&text) {
            let _ = writeln!(out, "| `{}` | {} |", path.display(), cell(&sentence));
        }
    }
    out.push('\n');
    out
}

fn styled(text: Option<&clap::builder::StyledStr>) -> String {
    text.map(ToString::to_string).unwrap_or_default()
}

fn about(command: &Command) -> String {
    styled(command.get_about())
}

fn long_about(command: &Command) -> String {
    let long = styled(command.get_long_about());
    let text = if long.is_empty() {
        about(command)
    } else {
        long
    };
    collapse(&text)
}

fn long_help(arg: &Arg) -> String {
    let long = styled(arg.get_long_help());
    if long.is_empty() {
        styled(arg.get_help())
    } else {
        long
    }
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Escape text for a Markdown table cell: pipes, and angle brackets outside
/// code spans (GitHub would read `<NAME>` as an HTML tag).
fn cell(text: &str) -> String {
    collapse(text)
        .split('`')
        .enumerate()
        .map(|(index, part)| {
            if index % 2 == 1 {
                part.replace('|', "\\|")
            } else {
                part.replace('|', "\\|")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;")
            }
        })
        .collect::<Vec<_>>()
        .join("`")
}

/// End help text with a full stop so a following note reads as a new sentence.
fn sentence(text: &str) -> String {
    if text.is_empty() || text.ends_with(['.', '!', '?', ')', '`']) {
        text.to_string()
    } else {
        format!("{text}.")
    }
}

fn arg_label(arg: &Arg) -> String {
    let mut forms = Vec::new();
    if let Some(short) = arg.get_short() {
        forms.push(format!("-{short}"));
    }
    if let Some(long) = arg.get_long() {
        forms.push(format!("--{long}"));
    }
    if let Some(aliases) = arg.get_visible_aliases() {
        forms.extend(aliases.into_iter().map(|alias| format!("--{alias}")));
    }
    if forms.is_empty() {
        format!("`{}`", value_name(arg))
    } else {
        format!("`{}`", forms.join("`, `"))
    }
}

fn value_form(arg: &Arg) -> String {
    let action = arg.get_action();
    if !action.takes_values() {
        return match action {
            ArgAction::Count => "count".to_string(),
            _ => "switch".to_string(),
        };
    }
    let possible: Vec<String> = arg
        .get_possible_values()
        .iter()
        .filter(|value| !value.is_hide_set())
        .map(|value| value.get_name().to_string())
        .collect();
    let value = if possible.is_empty() {
        format!("<{}>", value_name(arg))
    } else {
        possible.join("|")
    };
    let optional = arg
        .get_num_args()
        .is_some_and(|range| range.min_values() == 0);
    let mut form = if optional {
        format!("`[={value}]`")
    } else {
        format!("`{value}`")
    };
    if repeats(arg) {
        form.push_str(" (repeatable)");
    }
    form
}

fn default_value(arg: &Arg) -> String {
    if !arg.get_action().takes_values() {
        return String::new();
    }
    let values: Vec<String> = arg
        .get_default_values()
        .iter()
        .map(|value| value.to_string_lossy().into_owned())
        .collect();
    if values.is_empty() {
        String::new()
    } else {
        format!("`{}`", values.join(","))
    }
}

fn notes(command: &Command, arg: &Arg) -> String {
    let mut notes = Vec::new();
    if arg.is_required_set() {
        notes.push("Required.".to_string());
    }
    if arg.is_global_set() {
        notes.push("Global.".to_string());
    }
    if arg.is_hide_set() {
        notes.push("Hidden.".to_string());
    }
    let conflicts: Vec<String> = command
        .get_arg_conflicts_with(arg)
        .into_iter()
        .map(arg_label)
        .collect();
    if !conflicts.is_empty() {
        notes.push(format!("Conflicts with {}.", conflicts.join(", ")));
    }
    let help = sentence(&cell(&long_help(arg)));
    if help.is_empty() {
        notes.join(" ")
    } else if notes.is_empty() {
        help
    } else {
        format!("{help} {}", notes.join(" "))
    }
}

fn commands_section(commands: &[CommandPath<'_>]) -> String {
    let mut out = String::from(
        "## Commands\n\n\
         Every command accepts `-h`/`--help` unless its section says otherwise.\n\
         Hidden commands are internal (hooks, tests, the daemon) and are not part\n\
         of the supported interface.\n\n",
    );
    for path in commands {
        out.push_str(&command_block(path));
    }
    out
}

fn command_block(path: &CommandPath<'_>) -> String {
    let command = path.command;
    let mut out = String::new();
    let hidden = if path.hidden { " (hidden)" } else { "" };
    let _ = writeln!(out, "### `{}`{hidden}\n", path.display());
    let about = collapse(&about(command));
    if !about.is_empty() {
        let _ = writeln!(out, "{about}\n");
    }
    let usage = command.clone().render_usage().to_string();
    let usage = usage.trim().trim_start_matches("Usage:").trim();
    let _ = writeln!(out, "```text\n{usage}\n```\n");
    let aliases: Vec<&str> = command.get_all_aliases().collect();
    if !aliases.is_empty() {
        let _ = writeln!(out, "Aliases: {}\n", code_list(&aliases));
    }
    if !command.get_arguments().any(is_help_or_version) {
        out.push_str("No `--help`: every token after the verb goes to the command.\n\n");
    }
    out.push_str(&positional_table(command));
    out.push_str(&option_table(command));
    out.push_str(&group_lines(command));
    out.push_str(&subcommand_line(path));
    out
}

fn positional_table(command: &Command) -> String {
    let args = positionals(command);
    if args.is_empty() {
        return String::new();
    }
    let mut out = String::from("| Argument | Value | Default | Description |\n|---|---|---|---|\n");
    for arg in args {
        let name = if repeats(arg) {
            format!("`{}...`", value_name(arg))
        } else {
            format!("`{}`", value_name(arg))
        };
        let _ = writeln!(
            out,
            "| {name} | {} | {} | {} |",
            value_form(arg),
            default_value(arg),
            notes(command, arg)
        );
    }
    out.push('\n');
    out
}

fn option_table(command: &Command) -> String {
    let args = options(command);
    if args.is_empty() {
        return String::new();
    }
    let mut out = String::from("| Flag | Value | Default | Description |\n|---|---|---|---|\n");
    for arg in args {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} |",
            arg_label(arg),
            value_form(arg),
            default_value(arg),
            notes(command, arg)
        );
    }
    out.push('\n');
    out
}

/// Explicit argument groups. The derive's implicit one-group-per-struct is
/// `multiple` and constrains nothing, so it is skipped.
fn group_lines(command: &Command) -> String {
    let mut out = String::new();
    for group in command.get_groups() {
        if group.clone().is_multiple() && !group.is_required_set() {
            continue;
        }
        let members: Vec<String> = group
            .get_args()
            .filter_map(|id| command.get_arguments().find(|arg| arg.get_id() == id))
            .map(arg_label)
            .collect();
        let rule = if group.is_required_set() {
            "exactly one of"
        } else {
            "at most one of"
        };
        let _ = writeln!(
            out,
            "Group `{}`: {rule} {}.\n",
            group.get_id(),
            members.join(", ")
        );
    }
    if command.is_args_conflicts_with_subcommands_set() {
        out.push_str("Flags and subcommands are mutually exclusive here.\n\n");
    }
    out
}

fn subcommand_line(path: &CommandPath<'_>) -> String {
    let subs: Vec<String> = visible_subcommands(path.command)
        .map(|sub| format!("`{}`", sub.get_name()))
        .collect();
    if subs.is_empty() {
        return String::new();
    }
    let required = if path.command.is_subcommand_required_set() {
        "required"
    } else {
        "optional"
    };
    format!("Subcommands ({required}): {}.\n\n", subs.join(", "))
}

/// `ci/banned_python3.py` refuses the versioned interpreter name; here it is
/// the literal alias clud installs, so each such line carries the marker.
fn mark_interpreter_names(text: &str) -> String {
    let versioned = concat!("python", "3");
    text.lines()
        .map(|line| {
            if line.contains(versioned) {
                format!("{line} <!-- python-name-lint: allow -->")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMITTED: &str = include_str!("../../../docs/cli-grammar.md");

    #[test]
    fn committed_cli_grammar_is_current() {
        let rendered = render();
        if std::env::var_os(REGEN_ENV).is_some() {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join(DOC_PATH);
            std::fs::write(&path, &rendered).expect("write docs/cli-grammar.md");
            return;
        }
        let committed = COMMITTED.replace("\r\n", "\n");
        assert!(
            committed == rendered,
            "{DOC_PATH} is stale: the CLI changed without regenerating it. Run `{REGEN_COMMAND}` \
             and commit the result."
        );
    }

    #[test]
    fn every_top_level_command_word_has_a_section() {
        let rendered = render();
        for word in args::TOP_LEVEL_SUBCOMMANDS {
            assert!(
                rendered.contains(&format!("### `clud {word}`")),
                "no section for `clud {word}`"
            );
        }
    }

    #[test]
    fn separator_owners_and_multicall_names_are_rendered() {
        let rendered = render();
        for owner in args::SEPARATOR_OWNING_SUBCOMMANDS {
            assert!(rendered.contains(&format!("`{owner}`")));
        }
        for name in multicall::alias_names() {
            assert!(rendered.contains(&format!("| `{name}` |")), "{name}");
        }
    }

    #[test]
    fn table_cells_escape_pipes_and_tags_outside_code() {
        assert_eq!(cell("a | b <X> `c|<d>`"), "a \\| b &lt;X&gt; `c\\|<d>`");
    }

    #[test]
    fn exit_sentences_need_a_status() {
        let text = "Does a thing. Exits 1 when broken. Exit cleanly after use.";
        assert_eq!(
            exit_sentences(text),
            vec!["Exits 1 when broken.".to_string()]
        );
    }
}

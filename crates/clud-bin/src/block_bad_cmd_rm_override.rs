//! Protect user settings while distinguishing executable writes from data.
//! A protected name in an issue search or scratch heredoc is not a write.
//! Unknown operations that mention the protected setting still fail closed.

use std::io::Read;
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::shell_ast::{Invocation, Value};

/// Programs that only read their operands when run without a redirection.
const READ_ONLY: &[&str] = &[
    "cat", "less", "more", "head", "tail", "grep", "egrep", "fgrep", "rg", "jq", "wc", "ls",
    "stat", "file", "diff", "bat", "echo", "printf", "test", "[", "true",
];

pub(super) const REFUSAL: &str =
    "raw writes to user settings and safe_rm.extra_roots are refused. \
    Use `clud settings set /key JSON --reason TEXT` for an ordinary change, or \
    `clud settings roots JSON --reason TEXT` for a separate deletion-root change. \
    The user must approve the exact operation in its native desktop window; \
    environment variables, scripts, and claimed approval metadata are not authority";

pub(super) fn reason_for_payload(payload: &super::HookPayloadView) -> Option<String> {
    reason_for_payload_in(payload, crate::home::user_home())
}

fn reason_for_payload_in(
    payload: &super::HookPayloadView,
    home: Option<std::path::PathBuf>,
) -> Option<String> {
    let tool = payload
        .tool_name
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if super::block_bad_cmd_gate::gates_tool(&tool) {
        let dialect = super::shell_dialect_for_payload(payload);
        if let Some(verdict) =
            nested_patch_reason(&payload.command, dialect, &payload.cwd, home.as_deref())
        {
            return verdict;
        }
        return reason_in(
            &payload.command,
            dialect,
            Some(&payload.cwd),
            home.as_deref(),
        );
    }
    native_file_reason(payload, &tool, home.as_deref())
}

fn native_file_reason(
    payload: &super::HookPayloadView,
    tool: &str,
    home: Option<&Path>,
) -> Option<String> {
    if tool == "apply_patch" {
        return apply_patch_reason(payload, home);
    } else if matches!(tool, "write" | "edit" | "multiedit" | "notebookedit") {
        let path = payload.tool_input.as_ref().and_then(|input| {
            ["file_path", "notebook_path", "path"]
                .iter()
                .find_map(|key| input.get(key).and_then(serde_json::Value::as_str))
        });
        if path.is_none_or(|path| native_target_is_protected(path, payload, home)) {
            return Some(REFUSAL.into());
        }
    }
    None
}

/// Refusal for a patch whose destination cannot be established. Distinct from
/// [`REFUSAL`]: nothing here says the patch touches user settings, only that
/// it cannot be shown not to (#2289, #2222). Still fails closed. Every patch
/// refusal opens with a stable bracketed reason code (#2285).
pub(super) const UNRECOGNIZED_PAYLOAD: &str =
    "[unrecognized-payload] the apply_patch input has no recognizable patch text \
    (expected a string, or a `patch`/`input`/`cmd`/`command` field); refusing because \
    the target cannot be verified against the protected user settings file";

pub(super) const NO_TARGETS: &str =
    "[no-targets] the apply_patch text has no `*** Add/Update/Delete File:` or \
    `*** Move to:` header; refusing because the target cannot be verified \
    against the protected user settings file. Retry with a well-formed patch";

const NO_HOME: &str =
    "[no-home] the user home directory could not be resolved, so apply_patch targets cannot be \
    verified against the protected user settings file; refusing";

const PATCH_MARKERS: [&str; 4] = [
    "*** Add File: ",
    "*** Update File: ",
    "*** Delete File: ",
    "*** Move to: ",
];

/// Every string that may carry patch text in the shapes Codex sends: a bare
/// string, `{"patch"|"input"|"cmd"|"command": "<patch>"}`, or a `command` argv
/// array such as `["apply_patch", "<patch>"]`. All of them are collected so a
/// decoy candidate cannot hide a protected target.
fn patch_texts(input: &serde_json::Value) -> Vec<&str> {
    let mut texts = Vec::new();
    if let Some(text) = input.as_str() {
        texts.push(text);
    }
    for key in ["patch", "input", "cmd", "command"] {
        match input.get(key) {
            Some(serde_json::Value::String(text)) => texts.push(text),
            Some(serde_json::Value::Array(items)) => {
                texts.extend(items.iter().filter_map(serde_json::Value::as_str));
            }
            _ => {}
        }
    }
    texts
}

fn patch_targets(patch: &str) -> Vec<&str> {
    patch
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            PATCH_MARKERS
                .iter()
                .find_map(|prefix| line.strip_prefix(prefix))
        })
        .map(|path| path.trim_end_matches(['\r', '\n']))
        .collect()
}

/// Decide a patch from its candidate texts. The reason never echoes patch
/// content: only the protected file's own name.
fn patch_verdict(texts: &[&str], cwd: &str, home: Option<&Path>) -> Option<String> {
    if texts.is_empty() {
        return Some(UNRECOGNIZED_PAYLOAD.into());
    }
    let targets: Vec<&str> = texts.iter().copied().flat_map(patch_targets).collect();
    if targets.is_empty() {
        return Some(NO_TARGETS.into());
    }
    let Some(home) = home else {
        return Some(NO_HOME.into());
    };
    // Codex trims header lines, so the trimmed spelling is what gets written.
    targets
        .iter()
        .any(|path| protected_file(path, cwd, home) || protected_file(path.trim(), cwd, home))
        .then(|| protected_target_reason(home))
}

fn protected_target_reason(home: &Path) -> String {
    let name = crate::clud_settings::settings_path_at(home)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("[protected-target] the patch targets protected `{name}`; {REFUSAL}")
}

fn apply_patch_reason(payload: &super::HookPayloadView, home: Option<&Path>) -> Option<String> {
    let texts = payload
        .tool_input
        .as_ref()
        .map(patch_texts)
        .unwrap_or_default();
    patch_verdict(&texts, &payload.cwd.to_string_lossy(), home)
}

/// Codex may deliver `apply_patch` through a shell-style tool
/// (`apply_patch <<'EOF' ... EOF`, optionally after a `cd`). Such a command is
/// judged by the patch's targets, not by whether its text mentions the settings
/// file: a source file may legitimately discuss it. `None` means the command is
/// not a plain nested patch and the general shell rules apply.
///
/// Codex recognizes the heredoc form itself, with POSIX rules, before any
/// shell runs, so where `exec_command` defaults to the PowerShell dialect
/// (which has no heredocs) a POSIX parse is tried as well (#2378). That
/// fallback only accepts a patch that carries a heredoc: `<<` is a PowerShell
/// and cmd syntax error, so such a command never runs in those shells.
fn nested_patch_reason(
    command: &str,
    dialect: super::ShellDialect,
    cwd: &Path,
    home: Option<&Path>,
) -> Option<Option<String>> {
    nested_patch_reason_as(command, dialect, cwd, home, false).or_else(|| {
        (dialect != super::ShellDialect::Posix)
            .then(|| {
                let command = literal_backslash_cd(command);
                nested_patch_reason_as(&command, super::ShellDialect::Posix, cwd, home, true)
            })
            .flatten()
    })
}

/// A Windows `cd C:\dir && apply_patch` names its directory with backslashes
/// that Codex takes literally, while a POSIX parse would drop them as escapes
/// and lose the directory (#2378). Doubling them in a plain, unquoted `cd`
/// prefix keeps the path; any other command is returned unchanged.
fn literal_backslash_cd(command: &str) -> String {
    let trimmed = command.trim_start();
    let Some((prefix, rest)) = trimmed.split_once("&&") else {
        return command.to_string();
    };
    let plain = prefix.starts_with("cd ")
        && prefix.contains('\\')
        && !prefix.contains(['\n', '\'', '"', '$', '`']);
    if plain {
        format!("{}&&{rest}", prefix.replace('\\', "\\\\"))
    } else {
        command.to_string()
    }
}

fn nested_patch_reason_as(
    command: &str,
    dialect: super::ShellDialect,
    cwd: &Path,
    home: Option<&Path>,
    require_heredoc: bool,
) -> Option<Option<String>> {
    let script =
        super::block_bad_cmd_shell::parse_command_with_home(command, dialect, Some(cwd), home);
    if script.has_error {
        return None;
    }
    let patch = nested_patch_invocation(command, &script.invocations)?;
    if require_heredoc
        && !script
            .invocations
            .iter()
            .any(|invocation| invocation.heredoc.is_some())
    {
        return None;
    }
    let work = patch.cwd.as_literal()?;
    // The parser may attach the heredoc body to the `cd` that precedes the patch.
    let mut texts: Vec<&str> = script
        .invocations
        .iter()
        .filter_map(|invocation| invocation.heredoc.as_deref())
        .collect();
    texts.extend(patch.words.iter().skip(1).filter_map(Value::as_literal));
    Some(patch_verdict(&texts, work, home))
}

fn plain_redirects(invocation: &Invocation) -> bool {
    invocation.redirects.iter().all(|r| !r.writes_file())
}

/// Whether `cd` has a single literal target that bash resolves without
/// consulting `CDPATH`: absolute, `./`, `../`, `.` or `..`. Any other target
/// (`.clud`, `foo`) may land elsewhere than the tracked cwd.
fn cd_ignores_cdpath(cd: &Invocation) -> bool {
    let [_, target] = cd.effective_argv() else {
        return false;
    };
    let Some(target) = target.as_literal() else {
        return false;
    };
    target_ignores_cdpath(target)
}

/// Platform-aware core of [`cd_ignores_cdpath`]: bash skips `CDPATH` for any
/// target that begins with `/` or `.`; a drive-letter or UNC path is absolute.
fn target_ignores_cdpath(target: &str) -> bool {
    let bytes = target.as_bytes();
    let drive_absolute = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\');
    Path::new(target).is_absolute()
        || drive_absolute
        || target.starts_with("\\\\")
        || target.starts_with('/')
        || matches!(target, "." | "..")
        || [r"./", r"../", r".\", r"..\"]
            .iter()
            .any(|p| target.starts_with(p))
}

/// The `apply_patch` of a command that is exactly a bare patch or a single
/// `cd DIR && apply_patch`. Only `&&` guarantees the patch runs in the `cd`
/// directory the parser tracked: after `;`, a newline or `||` a failed `cd`
/// leaves the patch in the real cwd, so every other shape returns `None`.
fn nested_patch_invocation<'a>(
    command: &str,
    invocations: &'a [Invocation],
) -> Option<&'a Invocation> {
    let is_patch = |inv: &Invocation| {
        matches!(
            inv.effective_program().as_deref(),
            Some("apply_patch" | "applypatch")
        ) && inv.env.is_empty()
            && plain_redirects(inv)
    };
    match invocations {
        [patch] if is_patch(patch) => Some(patch),
        [cd, patch] if is_patch(patch) => {
            let is_cd = cd.effective_program().as_deref() == Some("cd")
                && cd.env.is_empty()
                && plain_redirects(cd)
                && cd_ignores_cdpath(cd);
            let rest = command.trim_start().strip_prefix(cd.source.trim())?;
            let rest = rest.trim_start().strip_prefix("&&")?;
            (is_cd && rest.trim_start().starts_with(patch.source.trim())).then_some(patch)
        }
        _ => None,
    }
}

fn native_target_is_protected(
    path: &str,
    payload: &super::HookPayloadView,
    home: Option<&Path>,
) -> bool {
    home.is_none_or(|home| protected_file(path, &payload.cwd.to_string_lossy(), home))
}

fn reason_in(
    command: &str,
    dialect: super::ShellDialect,
    cwd: Option<&Path>,
    home: Option<&Path>,
) -> Option<String> {
    let script = super::block_bad_cmd_shell::parse_command_with_home(command, dialect, cwd, home);
    let names_override = names_override(command, &script);
    let alts = pre_cd_cwds(command, &script.invocations, cwd);
    let writes_settings = script.invocations.iter().any(|invocation| {
        protected_operand(invocation, home, &alts)
            || invocation.redirects.iter().any(|redirect| {
                redirect.writes_file()
                    && home.is_none_or(|home| {
                        !safe_destination(&redirect.target, invocation, home, &alts)
                    })
            })
    });
    if writes_settings
        || (names_override
            && (script.has_error
                || home.is_none()
                || !script.invocations.iter().all(read_only_invocation)))
    {
        Some(REFUSAL.to_string())
    } else {
        None
    }
}

/// Lowercased text with quotes and backslashes dropped, so spellings such as
/// `.cl"ud"` compare equal to the plain name.
fn normalize_text(text: &str) -> String {
    text.to_ascii_lowercase()
        .chars()
        .filter(|c| !matches!(c, '\'' | '"' | '\\'))
        .collect()
}

/// Programs whose operands are only read and whose output never echoes an
/// operand name, so a protected name passed to them is not a write.
const CONTENT_READERS: &[&str] = &["cat", "head", "tail", "bat", "jq", "less", "more"];

/// Programs besides [`READ_ONLY`] that cannot interpret a heredoc body as
/// code. `tee` is still judged by its operands, never by the body.
const BODY_INERT: &[&str] = &["cd", "mkdir", "touch", "tee"];

fn program_in(invocation: &Invocation, names: &[&str]) -> bool {
    invocation.env.is_empty()
        && invocation
            .effective_program()
            .is_some_and(|program| names.contains(&program.as_str()))
}

fn body_inert(invocation: &Invocation) -> bool {
    invocation.words.is_empty()
        || (program_in(invocation, READ_ONLY) && !less_writes_log(invocation))
        || program_in(invocation, BODY_INERT)
}

/// Whether every heredoc marker in `command` has a quoted or escaped
/// delimiter, so no body is expanded.
fn heredocs_quoted(command: &str) -> bool {
    let command = command.replace("<<<", "");
    let quoted = command
        .split("<<")
        .skip(1)
        .filter(|rest| {
            let rest = rest.strip_prefix('-').unwrap_or(rest).trim_start();
            rest.starts_with(['\'', '"', '\\'])
        })
        .count();
    command.matches("<<").count() == quoted
}

/// `less` options that open a log file for writing (`-o`, `-O`,
/// `--log-file`, `--LOG-FILE`, and their unambiguous abbreviations).
fn less_writes_log(invocation: &Invocation) -> bool {
    if !program_in(invocation, &["less"]) {
        return false;
    }
    invocation.words.iter().skip(1).any(|word| {
        let text = word.text();
        if let Some(long) = text.strip_prefix("--") {
            let name = long
                .split('=')
                .next()
                .unwrap_or_default()
                .to_ascii_lowercase();
            return name.len() >= 2 && "log-file".starts_with(&name);
        }
        text.starts_with('-') && text.contains(['o', 'O'])
    })
}

/// A pure content reader whose operands are only read.
fn content_reader(invocation: &Invocation) -> bool {
    program_in(invocation, CONTENT_READERS) && !less_writes_log(invocation)
}

/// The byte range of the only occurrence of `needle` in `text[from..to]`.
fn unique_span(text: &str, needle: &str, from: usize, to: usize) -> Option<(usize, usize)> {
    let window = text.get(from..to)?;
    if needle.is_empty() || window.matches(needle).count() != 1 {
        return None;
    }
    let start = from + window.find(needle)?;
    Some((start, start + needle.len()))
}

/// Spans of the literal operands of a content reader, each located at its own
/// position inside the command's own source text (never by a global first
/// match). Any word that cannot be placed leaves the operands unscrubbed.
fn operand_spans(text: &str, invocation: &Invocation) -> Vec<(usize, usize)> {
    let Some((from, to)) = unique_span(text, &normalize_text(&invocation.source), 0, text.len())
    else {
        return Vec::new();
    };
    let mut cursor = from;
    let mut spans = Vec::new();
    for (index, (word, raw)) in invocation.words.iter().zip(&invocation.raw).enumerate() {
        let raw = normalize_text(raw);
        let Some(found) = text.get(cursor..to).and_then(|window| window.find(&raw)) else {
            return Vec::new();
        };
        let start = cursor + found;
        cursor = start + raw.len();
        if index > 0 && !raw.is_empty() && matches!(word, Value::Literal(_)) {
            spans.push((start, cursor));
        }
    }
    spans
}

/// Whether `reader`'s operands may be dropped: every command that could use
/// them as a write target (through `$_`, a later `"$@"` or the like) must be
/// body-inert, except other stages of the reader's own pipeline, which run in
/// subshells and never see its last argument. Commands in the reader's own
/// stage (`( cat F; sed -i x "$_" ) | cat`, `{ ...; } | cat`) run
/// sequentially in one shell, so `$_` carries the path and they get no
/// exemption.
fn operands_scrubbable(reader: &Invocation, script: &crate::shell_ast::Script) -> bool {
    script.invocations.iter().all(|other| {
        body_inert(other)
            || reader.pipeline.is_some_and(|slot| {
                other
                    .pipeline
                    .is_some_and(|o| o.id == slot.id && o.stage != slot.stage)
            })
    })
}

/// `text` with the heredoc bodies and read-only operands that cannot name a
/// write target removed.
fn scrubbed(text: &str, command: &str, script: &crate::shell_ast::Script) -> String {
    let quoted = heredocs_quoted(command);
    let all_inert = script.invocations.iter().all(body_inert);
    let mut cuts: Vec<(usize, usize)> = Vec::new();
    for invocation in &script.invocations {
        let body = invocation.heredoc.as_deref();
        let body = body.filter(|body| all_inert && (quoted || !body.contains(['$', '`'])));
        if let Some(body) = body {
            cuts.extend(unique_span(text, &normalize_text(body), 0, text.len()));
        }
        if content_reader(invocation) && operands_scrubbable(invocation, script) {
            cuts.extend(operand_spans(text, invocation));
        }
    }
    cuts.sort_unstable();
    let mut out = String::new();
    let mut at = 0;
    for (start, end) in cuts {
        if start >= at {
            out.push_str(&text[at..start]);
            at = end;
        }
    }
    out.push_str(&text[at..]);
    out
}

/// Whether the command names the protected settings (the home-reaching
/// settings path, or `extra_roots`) anywhere that could matter. Heredoc
/// bodies and literal operands of pure content readers are dropped (#2353),
/// but only when every command that could reuse an operand (through `$_`) is
/// body-inert or a stage of the same pipeline, and bodies only when every command is body-inert, since
/// the parser attaches a body to the first command of a list. Each operand is removed at its own
/// position, never by first match. Every other word, including any the
/// parser could not evaluate, keeps counting, and a parse error scrubs
/// nothing.
fn names_override(command: &str, script: &crate::shell_ast::Script) -> bool {
    let mut text = normalize_text(command);
    if !script.has_error {
        text = scrubbed(&text, command, script);
    }
    // A repo's own `.clud/settings.json` is never read for the override, so
    // only a spelling that can reach the user's home counts.
    let reaches_home = ["~", "home", "userprofile", "/users/", "/root/"]
        .iter()
        .any(|marker| text.contains(marker));
    text.contains("extra_roots")
        || (text.contains(".clud") && text.contains("settings.json") && reaches_home)
}

/// Working directories a command may really run in besides the one the parser
/// tracked. The parser assumes every `cd` succeeded, but after `;`, a newline,
/// `||`, `&` or a pipe a failed `cd` leaves the next command in the old
/// directory (#2348). Only a chain made purely of `&&` is certain, so any other
/// connector next to a `cd` makes the pre-`cd` and every intermediate cwd
/// candidates as well; a path is refused if it resolves to the protected file
/// under any of them.
fn pre_cd_cwds(command: &str, invocations: &[Invocation], cwd: Option<&Path>) -> Vec<String> {
    let has_cd = invocations
        .iter()
        .any(|inv| inv.effective_program().as_deref() == Some("cd"));
    if !has_cd || !command.replace("&&", "").contains([';', '\n', '|', '&']) {
        return Vec::new();
    }
    let mut cwds: Vec<String> = cwd
        .map(|cwd| cwd.to_string_lossy().into_owned())
        .into_iter()
        .collect();
    for inv in invocations {
        if let Some(literal) = inv.cwd.as_literal() {
            if !cwds.iter().any(|known| known == literal) {
                cwds.push(literal.to_string());
            }
        }
    }
    cwds
}

fn protected_operand(invocation: &Invocation, home: Option<&Path>, alts: &[String]) -> bool {
    if read_only_invocation(invocation) {
        return false;
    }
    let Some(home) = home else {
        return false;
    };
    let mutator = matches!(
        invocation.effective_program().as_deref(),
        Some("cp" | "mv" | "tee" | "install" | "dd" | "set-content" | "add-content" | "out-file")
    );
    let Some(cwd) = invocation.cwd.as_literal() else {
        return mutator || names_protected_file(invocation, home);
    };
    invocation.effective_argv().iter().skip(1).any(|word| {
        word.as_literal().map_or(mutator, |word| {
            // A known protected operand on an unknown writer is refused too:
            // passing a target to a script does not establish its authority.
            let word = word.strip_prefix("of=").unwrap_or(word);
            std::iter::once(cwd)
                .chain(alts.iter().map(String::as_str))
                .any(|cwd| operand_hits(word, cwd, home))
        })
    })
}

/// Whether `word` resolves, or as a glob/brace pattern could resolve, to the
/// protected file when run from `cwd`.
fn operand_hits(word: &str, cwd: &str, home: &Path) -> bool {
    protected_file(word, cwd, home) || (has_glob(word) && pattern_may_match(word, cwd, home))
}

fn has_glob(word: &str) -> bool {
    word.contains(['*', '?', '[', '{'])
}

/// Expand `{a,b}` groups. `None` when the expansion is too large to enumerate,
/// which callers treat as a match.
fn expand_braces(word: &str) -> Option<Vec<String>> {
    const LIMIT: usize = 256;
    let mut done = Vec::new();
    let mut pending = vec![word.to_string()];
    while let Some(text) = pending.pop() {
        let text = wildcard_sequences(&text);
        let Some((open, close, commas)) = first_brace_group(&text) else {
            done.push(text);
            continue;
        };
        let (head, tail) = (&text[..open], &text[close + 1..]);
        let mut start = open + 1;
        for end in commas.into_iter().chain([close]) {
            pending.push(format!("{head}{}{tail}", &text[start..end]));
            start = end + 1;
        }
        if pending.len() + done.len() > LIMIT {
            return None;
        }
    }
    Some(done)
}

/// Byte offsets of the first balanced `{...,...}` group: open, close, and its
/// top-level commas. A group with no comma is literal text in bash.
fn first_brace_group(text: &str) -> Option<(usize, usize, Vec<usize>)> {
    let mut from = 0;
    while let Some(offset) = text[from..].find('{') {
        let open = from + offset;
        let (mut depth, mut commas) = (0_usize, Vec::new());
        for (index, ch) in text[open..].char_indices() {
            match ch {
                '{' => depth += 1,
                ',' if depth == 1 => commas.push(open + index),
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        if !commas.is_empty() {
                            return Some((open, open + index, commas));
                        }
                        break;
                    }
                }
                _ => {}
            }
        }
        from = open + 1;
    }
    None
}

/// Glob match where `*` and `?` never cross a `/` and `[...]` is a class.
/// Iterative star-backtracking, O(n*m) worst case.
fn glob_match(pattern: &[char], text: &[char]) -> bool {
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ti < text.len() {
        if pattern.get(pi) == Some(&'*') {
            while pattern.get(pi) == Some(&'*') {
                pi += 1;
            }
            star = Some((pi, ti));
        } else if let Some(next) = glob_step(pattern, pi, text[ti]) {
            pi = next;
            ti += 1;
        } else {
            match star {
                Some((sp, st)) if text[st] != '/' => {
                    star = Some((sp, st + 1));
                    pi = sp;
                    ti = st + 1;
                }
                _ => return false,
            }
        }
    }
    pattern[pi.min(pattern.len())..].iter().all(|&c| c == '*')
}

/// Match one non-`*` pattern element at `pi` against `c`; the next pattern
/// index on success.
fn glob_step(pattern: &[char], pi: usize, c: char) -> Option<usize> {
    let &p = pattern.get(pi)?;
    match p {
        '?' => (c != '/').then_some(pi + 1),
        '[' => {
            let rest = &pattern[pi + 1..];
            match class_end(rest) {
                Some(end) => (c != '/' && class_has(&rest[..end], c)).then_some(pi + end + 2),
                // An unterminated class is a literal `[`.
                None => (c == '[').then_some(pi + 1),
            }
        }
        _ => (c == p).then_some(pi + 1),
    }
}

/// Index of the `]` closing a class whose `[` was just consumed. A `[:name:]`
/// element is skipped whole so its inner `]` does not end the class.
fn class_end(after_open: &[char]) -> Option<usize> {
    let skip = usize::from(matches!(after_open.first(), Some('!' | '^')));
    let mut i = skip + usize::from(after_open.get(skip) == Some(&']'));
    while i < after_open.len() {
        match after_open[i] {
            ']' => return Some(i),
            '[' if after_open.get(i + 1) == Some(&':') => {
                let found = after_open[i + 2..].windows(2).position(|w| w == [':', ']']);
                i += found.map_or(1, |n| n + 4);
            }
            _ => i += 1,
        }
    }
    None
}

fn class_has(class: &[char], c: char) -> bool {
    // POSIX classes (`[:alpha:]`) are not evaluated: match any char, which
    // can only over-refuse.
    if class.windows(2).any(|w| w == ['[', ':']) {
        return true;
    }
    let (negate, class) = match class.split_first() {
        Some((&('!' | '^'), rest)) => (true, rest),
        _ => (false, class),
    };
    let mut hit = false;
    let mut i = 0;
    while i < class.len() {
        if class.get(i + 1) == Some(&'-') && i + 2 < class.len() {
            hit |= (class[i]..=class[i + 2]).contains(&c);
            i += 3;
        } else {
            hit |= class[i] == c;
            i += 1;
        }
    }
    hit != negate
}

/// Replace each comma-free `{x..y}` sequence group with `*` (over-refusal is
/// fine); other text is untouched.
fn wildcard_sequences(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find(['{', '}']) {
            Some(close) if after[close..].starts_with('}') && after[..close].contains("..") => {
                if !after[..close].contains(',') {
                    out.push('*');
                    rest = &after[close + 1..];
                    continue;
                }
                out.push('{');
                rest = after;
            }
            _ => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn pattern_may_match(word: &str, cwd: &str, home: &Path) -> bool {
    let Some(patterns) = expand_braces(word) else {
        return true;
    };
    let settings = crate::clud_settings::settings_path_at(home);
    let lower = |path: &Path| -> Vec<char> {
        crate::path_norm::display_slash(path)
            .to_ascii_lowercase()
            .chars()
            .collect()
    };
    let targets = [
        lower(&settings),
        lower(&canonical_creation_target(&settings)),
    ];
    patterns.iter().any(|pattern| {
        let expanded = match pattern
            .strip_prefix("~/")
            .or_else(|| pattern.strip_prefix("~\\"))
        {
            Some(relative) => home.join(relative).to_string_lossy().into_owned(),
            None => pattern.clone(),
        };
        let joined = crate::shell_ast::join_path(cwd, &expanded);
        let joined: Vec<char> = crate::path_norm::display_slash(Path::new(&joined))
            .to_ascii_lowercase()
            .chars()
            .collect();
        targets.iter().any(|target| glob_match(&joined, target))
    })
}

/// With the cwd unknown a relative spelling cannot be resolved, so any word
/// whose final component is the protected file name fails closed (#2348).
fn names_protected_file(invocation: &Invocation, home: &Path) -> bool {
    let settings = crate::clud_settings::settings_path_at(home);
    let Some(name) = settings
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
    else {
        return true;
    };
    invocation.effective_argv().iter().skip(1).any(|word| {
        let raw = match word {
            Value::Literal(text) => text.as_str(),
            Value::Unknown(unknown) => unknown.raw.as_str(),
        };
        let raw = raw.trim().trim_matches(['\'', '"']).to_lowercase();
        let raw = raw.strip_prefix("of=").unwrap_or(&raw);
        let last = raw.rsplit(['/', '\\']).next().unwrap_or(raw);
        let last = last.trim_matches(['\'', '"']);
        last == name
            || (has_glob(last) && {
                let name: Vec<char> = name.chars().collect();
                expand_braces(last).is_none_or(|options| {
                    options
                        .iter()
                        .any(|option| glob_match(&option.chars().collect::<Vec<_>>(), &name))
                })
            })
    })
}

fn safe_destination(target: &Value, invocation: &Invocation, home: &Path, alts: &[String]) -> bool {
    let Some(target) = target.as_literal() else {
        return false;
    };
    if target == "/dev/null" {
        return true;
    }
    let Some(cwd) = invocation.cwd.as_literal() else {
        return false;
    };
    !std::iter::once(cwd)
        .chain(alts.iter().map(String::as_str))
        .any(|cwd| protected_file(target, cwd, home))
}

fn protected_file(target: &str, cwd: &str, home: &Path) -> bool {
    let expanded = if let Some(relative) = target
        .strip_prefix("~/")
        .or_else(|| target.strip_prefix("~\\"))
    {
        home.join(relative).to_string_lossy().into_owned()
    } else {
        target.to_string()
    };
    let destination = if crate::platform::host::WINDOWS || !expanded.contains('\\') {
        Path::new(cwd).join(&expanded)
    } else {
        crate::shell_ast::join_path(cwd, &expanded).into()
    };
    let settings = crate::clud_settings::settings_path_at(home);
    let canonical = |path: &Path| {
        let path = crate::path_norm::win32_file_name(path);
        crate::path_norm::display_slash(&canonical_creation_target(&path)).to_ascii_lowercase()
    };
    // A `..` after a missing directory cannot be canonicalized on disk, so the
    // lexically collapsed spelling is compared too (#2348).
    let lexical: std::path::PathBuf = crate::shell_ast::join_path(cwd, &expanded).into();
    let settings = canonical(&settings);
    canonical(&destination) == settings || canonical(&lexical) == settings
}

fn canonical_creation_target(path: &Path) -> std::path::PathBuf {
    let mut ancestor = path;
    let mut missing = Vec::new();
    loop {
        if let Ok(mut canonical) = crate::path_norm::canonicalize_plain(ancestor) {
            for component in missing.iter().rev() {
                canonical.push(component);
            }
            return Path::new(&crate::shell_ast::join_path(
                "",
                &canonical.to_string_lossy(),
            ))
            .into();
        }
        let (Some(parent), Some(component)) = (ancestor.parent(), ancestor.file_name()) else {
            return path.to_path_buf();
        };
        missing.push(component);
        ancestor = parent;
    }
}

fn read_only_invocation(invocation: &Invocation) -> bool {
    let words = invocation.effective_argv();
    let Some(program) = words.first() else {
        return true;
    };
    let Some(program) = program.as_literal() else {
        return false;
    };
    let name = super::block_bad_cmd_shell::program_name(program);
    (READ_ONLY.contains(&name.as_str()) && !less_writes_log(invocation))
        || program == "["
        || forge_read(&name, words)
        || git_read(&name, words)
        || numeric_sed_read(&name, words)
        || controlled_settings_command(invocation)
}

fn controlled_settings_command(invocation: &Invocation) -> bool {
    let words = &invocation.words;
    if !invocation.env.is_empty()
        || words.get(1).and_then(Value::as_literal) != Some("settings")
        || !matches!(
            words.get(2).and_then(Value::as_literal),
            Some("set" | "roots")
        )
    {
        return false;
    }
    let Some(program) = words.first().and_then(Value::as_literal) else {
        return false;
    };
    if super::block_bad_cmd_shell::program_name(program) != "clud" {
        return false;
    }
    let Ok(current) = std::env::current_exe() else {
        return false;
    };
    let candidate = if program.contains(['/', '\\']) {
        invocation
            .cwd
            .as_literal()
            .map(|cwd| std::path::PathBuf::from(crate::shell_ast::join_path(cwd, program)))
    } else {
        std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .flat_map(|directory| {
                    [
                        directory.join(program),
                        directory.join(format!("{program}.exe")),
                    ]
                })
                .find(|path| path.is_file())
        })
    };
    let Some(candidate) = candidate else {
        return false;
    };
    if matches!(
        (crate::path_norm::canonicalize_plain(&candidate), crate::path_norm::canonicalize_plain(&current)),
        (Ok(candidate), Ok(current)) if candidate == current
    ) {
        return true;
    }
    if std::fs::metadata(&candidate)
        .ok()
        .map(|metadata| metadata.len())
        != std::fs::metadata(&current)
            .ok()
            .map(|metadata| metadata.len())
    {
        return false;
    }
    // Installed and runtime-cache copies can have different paths. Only an
    // exact copy of this scanner's executable gets the controlled exception.
    matches!((executable_digest(&candidate), executable_digest(&current)), (Ok(candidate), Ok(current)) if candidate == current)
}

fn executable_digest(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut file = std::fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(digest.finalize().into())
}

/// `gh issue|pr` verbs that only talk to the forge (#2454): their text
/// operands (`--comment`, `--body`, `--title`) are data, never a local write.
fn forge_read(program: &str, words: &[Value]) -> bool {
    program == "gh"
        && matches!(
            words.get(1).and_then(Value::as_literal),
            Some("issue" | "pr")
        )
        && matches!(
            words.get(2).and_then(Value::as_literal),
            Some("list" | "view" | "comment" | "create" | "edit" | "close" | "reopen")
        )
}

/// Git subcommands that read history or the index (#2454). A path operand
/// (`git log -- <file>`) names what to read; `--output` is the one way these
/// write a file, so it disqualifies.
const GIT_READS: &[&str] = &[
    "log",
    "show",
    "diff",
    "blame",
    "status",
    "grep",
    "ls-files",
    "rev-parse",
    "describe",
];

fn git_read(program: &str, words: &[Value]) -> bool {
    if program != "git" {
        return false;
    }
    let mut rest = words.iter().skip(1).map(Value::as_literal);
    let subcommand = loop {
        match rest.next() {
            Some(Some("-C" | "-c")) => {
                rest.next();
            }
            Some(Some(word)) if word.starts_with('-') => {}
            Some(word) => break word,
            None => return false,
        }
    };
    subcommand.is_some_and(|sub| GIT_READS.contains(&sub))
        && words
            .iter()
            .filter_map(Value::as_literal)
            .all(|word| !word.starts_with("--output"))
}

fn numeric_sed_read(program: &str, words: &[Value]) -> bool {
    program == "sed"
        && words.get(1).and_then(Value::as_literal) == Some("-n")
        && words
            .get(2)
            .and_then(Value::as_literal)
            .and_then(|script| script.strip_suffix('p'))
            .is_some_and(|range| {
                range
                    .chars()
                    .all(|c| c.is_ascii_digit() || matches!(c, ',' | '$'))
            })
        && words.iter().skip(3).all(|word| {
            word.as_literal()
                .is_some_and(|operand| !operand.starts_with('-'))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_mutators_cannot_write_home_settings() {
        let home = tempfile::tempdir().unwrap();
        for command in [
            "cp staged.json .clud/settings.json",
            "mv staged.json .clud/settings.json",
            "tee .clud/settings.json < staged.json",
        ] {
            assert!(
                reason_in(
                    command,
                    crate::block_bad_cmd::ShellDialect::Posix,
                    Some(home.path()),
                    Some(home.path()),
                )
                .is_some(),
                "{command}"
            );
        }
    }

    fn relative_reason(home: &Path, command: &str) -> Option<String> {
        let cwd = home.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        reason_in(
            command,
            crate::block_bad_cmd::ShellDialect::Posix,
            Some(&cwd),
            Some(home),
        )
    }

    #[test]
    fn relative_spellings_into_home_settings_are_refused() {
        let home = tempfile::tempdir().unwrap();
        let name = crate::clud_settings::settings_path_at(home.path())
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        for command in [
            format!("echo x > ../.clud/{name}"),
            format!("cd proj && echo x > ../../x/../.clud/{name}"),
            format!("cp a ../.clud/{name}"),
            format!("tee ../.clud/{name}"),
            format!("mv a ./../.clud/{name}"),
            format!("cd .. && echo x > .clud/{name}"),
        ] {
            assert!(
                relative_reason(home.path(), &command).is_some(),
                "{command}"
            );
        }
    }

    fn settings_name(home: &Path) -> String {
        crate::clud_settings::settings_path_at(home)
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    #[test]
    fn failed_cd_before_a_non_and_connector_keeps_the_old_cwd() {
        let home = tempfile::tempdir().unwrap();
        let name = settings_name(home.path());
        for command in [
            format!("cd /nonexistent; echo x > ../.clud/{name}"),
            format!("cd /nonexistent\necho x > ../.clud/{name}"),
            format!("cd /nonexistent || echo x > ../.clud/{name}"),
            format!("cd /nonexistent & echo x > ../.clud/{name}"),
            format!("cd /nonexistent; cp a ../.clud/{name}"),
            format!("cd /nonexistent && true; cp a ../.clud/{name}"),
        ] {
            assert!(
                relative_reason(home.path(), &command).is_some(),
                "{command}"
            );
        }
        let unrelated = format!("cd /nonexistent; echo x > ../other/{name}");
        assert_eq!(relative_reason(home.path(), &unrelated), None);
    }

    #[test]
    fn globs_and_braces_that_can_match_the_protected_file_are_refused() {
        let home = tempfile::tempdir().unwrap();
        let name = settings_name(home.path());
        let stem = name.trim_end_matches(".json");
        for command in [
            "cp a ../.clud/*".to_string(),
            "sed -i s/a/b/ ../.clud/*".to_string(),
            format!("tee ../.clud/{{{stem},x}}.json"),
            format!("cp a ../.clud/{stem}.js?n"),
            format!("cp a ../.clud/{stem}.[j]son"),
            format!("cp a ../.c*/{name}"),
            format!("cp a ../.clu[[:alpha:]]/{name}"),
            format!("cp a ../.clu{{c..e}}/{name}"),
            format!("cp a ../.clud/{{{stem}.j,x}}{{a..c}}son"),
        ] {
            assert!(
                relative_reason(home.path(), &command).is_some(),
                "{command}"
            );
        }
    }

    #[test]
    fn glob_match_is_not_exponential_on_star_runs() {
        let pattern: Vec<char> = "*".repeat(40).chars().chain(['x']).collect();
        let text: Vec<char> = "a".repeat(60).chars().collect();
        let started = std::time::Instant::now();
        assert!(!glob_match(&pattern, &text));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        let name: Vec<char> = "ax".chars().collect();
        assert!(glob_match(&pattern, &name));
        assert!(!glob_match(&['*', 'x'], &['a', '/', 'x']));
    }

    #[test]
    fn unrelated_globs_and_lookalike_names_are_allowed() {
        let home = tempfile::tempdir().unwrap();
        let name = settings_name(home.path());
        for command in [
            "cp a ../other/*".to_string(),
            "cp a ../.clud/other-*.txt".to_string(),
            "tee ../other/{a,b}.json".to_string(),
            "rm -rf target/*".to_string(),
            "cp src/*.rs dst/".to_string(),
            "cp a ../other/[[:alpha:]]*".to_string(),
            format!("cp a ../.clud/foo{name}"),
            format!("cd \"$X\"; sed -i s/a/b/ foo{name}"),
            format!("cd \"$X\"; sed -i s/a/b/ ../.clud/{name}x"),
        ] {
            assert_eq!(relative_reason(home.path(), &command), None, "{command}");
        }
        let unknown = format!("cd \"$X\"; sed -i s/a/b/ ../.clud/{name}");
        assert!(relative_reason(home.path(), &unknown).is_some());
    }

    #[test]
    fn relative_reads_and_unrelated_paths_are_allowed() {
        let home = tempfile::tempdir().unwrap();
        let name = crate::clud_settings::settings_path_at(home.path())
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        for command in [
            format!("cat ../.clud/{name}"),
            format!("cp a ../other/{name}"),
            format!("grep x ../.clud/{name}"),
            format!("echo x > ../other/{name}"),
        ] {
            assert_eq!(relative_reason(home.path(), &command), None, "{command}");
        }
    }

    #[test]
    fn reads_and_prose_that_name_the_settings_file_are_allowed() {
        let home = tempfile::tempdir().unwrap();
        let name = settings_name(home.path());
        for command in [
            format!("cat .clud/{name} 2>/dev/null"),
            format!("cat /home/dev/repo/.clud/{name} 2>/dev/null"),
            format!("cat ~/repo/.clud/{name} 2>/dev/null"),
            format!("cat > scratchpad/issues/x.md <<'EOF'\nsee .clud/{name} in ~/home\nEOF"),
            format!("cat > scratchpad/issues/x.md <<'EOF'\nsee ~/.clud/{name}\nEOF"),
            format!("cat > scratchpad/issues/x.md <<EOF\nsee ~/.clud/{name}\nEOF"),
            format!("cat ~/.clud/{name} > /tmp/x"),
            format!("cat ~/r/.clud/{name} 2>/dev/null | python -c 'import sys'"),
            format!("mkdir -p s/i && cat > s/i/x.md <<'EOF'\nsee ~/.clud/{name}\nEOF"),
        ] {
            assert_eq!(relative_reason(home.path(), &command), None, "{command}");
        }
    }

    #[test]
    fn writes_hidden_near_prose_or_reads_are_still_refused() {
        let home = tempfile::tempdir().unwrap();
        let name = settings_name(home.path());
        for command in [
            format!("cat > ~/.clud/{name} <<'EOF'\nsee ~/.clud/{name}\nEOF"),
            format!("cat > x.md <<EOF\n$(echo x > ~/.clud/{name})\nEOF"),
            format!("cat > x.md <<EOF\n`echo x > ~/.clud/{name}`\nEOF"),
            format!("cp /tmp/x ~/.clud/{name}"),
            format!("cat a && python - <<'EOF'\nopen('~/.clud/{name}', 'w')\nEOF"),
            format!("python - <<EOF\nopen('~/.clud/{name}', 'w')\nEOF"),
            format!("echo ~/.clud/{name} | xargs cp /tmp/x"),
            format!("X=~/.clud/{name}; cp /tmp/x $X"),
            format!("cat $(python -c \"open('~/.clud/{name}','w')\")"),
            format!("cat ~/.clud/{name} 2>/dev/null > ~/.clud/{name}"),
        ] {
            assert!(
                relative_reason(home.path(), &command).is_some(),
                "{command}"
            );
        }
    }

    #[test]
    fn missing_settings_through_parent_alias_are_protected() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("home");
        std::fs::create_dir(&home).unwrap();
        let alias = crate::platform::path_norm::test_alias(&home, &directory.path().join("alias"));
        assert!(protected_file(
            &alias.join(".clud/settings.json").to_string_lossy(),
            &directory.path().to_string_lossy(),
            &home,
        ));
    }

    fn patch_payload(
        cwd: &Path,
        input: serde_json::Value,
    ) -> crate::block_bad_cmd::HookPayloadView {
        crate::block_bad_cmd::HookPayloadView {
            tool_name: "apply_patch".into(),
            command: String::new(),
            cwd: cwd.to_path_buf(),
            tool_input: Some(input),
            agent_id: None,
            agent_type: None,
            session_id: None,
            transcript_path: None,
        }
    }

    fn patch_reason(home: &Path, input: serde_json::Value) -> Option<String> {
        native_file_reason(&patch_payload(home, input), "apply_patch", Some(home))
    }

    const ORDINARY: &str = "*** Begin Patch\n*** Update File: memories/MEMORY.md\n@@\n-a\n+b\n\
        *** Update File: profiles/x/memories/MEMORY.md\n@@\n-a\n+b\n\
        *** Add File: src/new.rs\n+fn main() {}\n*** End Patch";

    #[test]
    fn ordinary_patches_are_allowed_in_every_payload_shape() {
        let home = tempfile::tempdir().unwrap();
        for input in [
            serde_json::json!(ORDINARY),
            serde_json::json!({"patch": ORDINARY}),
            serde_json::json!({"input": ORDINARY}),
            serde_json::json!({"command": ORDINARY}),
            serde_json::json!({"command": ["apply_patch", ORDINARY]}),
            serde_json::json!({"command": ["apply_patch", ORDINARY, "extra"]}),
        ] {
            assert_eq!(patch_reason(home.path(), input.clone()), None, "{input}");
        }
    }

    #[test]
    fn crlf_and_indented_marker_lines_are_understood() {
        let home = tempfile::tempdir().unwrap();
        let crlf = ORDINARY.replace('\n', "\r\n");
        assert_eq!(patch_reason(home.path(), serde_json::json!(crlf)), None);
        let indented = ORDINARY.replace("\n*** ", "\n  *** ");
        assert_eq!(patch_reason(home.path(), serde_json::json!(indented)), None);
        let settings = crate::clud_settings::settings_path_at(home.path());
        let protected = format!(
            "*** Begin Patch\r\n  *** Update File: {}\r\n@@\r\n-a\r\n+b\r\n*** End Patch",
            settings.display()
        );
        assert_protected(patch_reason(home.path(), serde_json::json!(protected)));
    }

    #[test]
    fn relative_and_absolute_ordinary_paths_are_allowed() {
        let home = tempfile::tempdir().unwrap();
        let absolute = home.path().join("repo/src/lib.rs");
        let patch = format!(
            "*** Begin Patch\n*** Update File: ./src/lib.rs\n@@\n-a\n+b\n\
             *** Update File: {}\n@@\n-a\n+b\n*** End Patch",
            absolute.display()
        );
        assert_eq!(patch_reason(home.path(), serde_json::json!(patch)), None);
    }

    #[test]
    fn patches_touching_the_protected_file_are_refused() {
        let home = tempfile::tempdir().unwrap();
        let settings = crate::clud_settings::settings_path_at(home.path());
        let shown = settings.display();
        let relative = settings
            .strip_prefix(home.path())
            .unwrap()
            .display()
            .to_string();
        for patch in [
            format!("*** Begin Patch\n*** Update File: {shown}\n@@\n-a\n+b\n*** End Patch"),
            format!("*** Begin Patch\n*** Update File: {relative}\n@@\n-a\n+b\n*** End Patch"),
            format!("*** Begin Patch\n*** Delete File: {shown}\n*** End Patch"),
            format!(
                "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-a\n+b\n\
                 *** Update File: {shown}\n@@\n-a\n+b\n*** End Patch"
            ),
            format!(
                "*** Begin Patch\n*** Update File: src/a.rs\n*** Move to: {shown}\n@@\n-a\n+b\n*** End Patch"
            ),
        ] {
            for input in [
                serde_json::json!(patch),
                serde_json::json!({"command": ["apply_patch", patch]}),
            ] {
                assert_protected(patch_reason(home.path(), input));
            }
        }
    }

    #[test]
    fn trailing_whitespace_on_a_protected_target_is_refused() {
        let home = tempfile::tempdir().unwrap();
        let settings = crate::clud_settings::settings_path_at(home.path());
        for pad in [" ", "\t", "  \t "] {
            let patch = format!(
                "*** Begin Patch\n*** Update File: {}{pad}\n@@\n-a\n+b\n*** End Patch",
                settings.display()
            );
            assert_protected(patch_reason(home.path(), serde_json::json!(patch)));
        }
    }

    #[test]
    fn decoy_candidates_do_not_hide_a_protected_target() {
        let home = tempfile::tempdir().unwrap();
        let settings = crate::clud_settings::settings_path_at(home.path());
        let protected = format!("*** Update File: {}\n@@\n-a\n+b", settings.display());
        let wrapped = format!("*** Begin Patch\n{protected}\n*** End Patch");
        for input in [
            serde_json::json!({"patch": ORDINARY, "command": wrapped}),
            serde_json::json!({"input": ORDINARY, "patch": wrapped}),
            serde_json::json!({"command": ["apply_patch", ORDINARY, wrapped]}),
            serde_json::json!({"command": ["apply_patch", ORDINARY, protected]}),
        ] {
            assert_protected(patch_reason(home.path(), input));
        }
    }

    #[test]
    fn multiple_ordinary_candidates_are_still_allowed() {
        let home = tempfile::tempdir().unwrap();
        let other = "*** Begin Patch\n*** Add File: src/other.rs\n+x\n*** End Patch";
        for input in [
            serde_json::json!({"patch": ORDINARY, "command": other}),
            serde_json::json!({"command": ["apply_patch", ORDINARY, other]}),
        ] {
            assert_eq!(patch_reason(home.path(), input.clone()), None, "{input}");
        }
    }

    fn assert_protected(got: Option<String>) {
        let got = got.expect("protected target must be refused");
        assert!(got.starts_with("[protected-target]"), "{got}");
        assert!(
            got.contains("settings.json") && got.contains(REFUSAL),
            "{got}"
        );
    }

    #[test]
    fn unrecognized_payloads_fail_closed_with_their_own_code() {
        let home = tempfile::tempdir().unwrap();
        for input in [
            serde_json::json!({}),
            serde_json::json!(42),
            serde_json::json!({"command": []}),
            serde_json::json!({"command": [1, 2]}),
        ] {
            let got = patch_reason(home.path(), input.clone()).expect("fail closed");
            assert_eq!(got, UNRECOGNIZED_PAYLOAD, "{input}");
            assert!(!got.contains("extra_roots"), "{input}");
        }
    }

    #[test]
    fn headerless_patches_fail_closed_with_their_own_code() {
        let home = tempfile::tempdir().unwrap();
        for input in [
            serde_json::json!("*** Begin Patch\n@@\n+No target header\n*** End Patch"),
            serde_json::json!({"cmd": "apply_patch <<'EOF'\n@@\n+x\nEOF"}),
        ] {
            let got = patch_reason(home.path(), input.clone()).expect("fail closed");
            assert_eq!(got, NO_TARGETS, "{input}");
            assert!(!got.contains("+No target"), "{input}");
        }
    }

    /// Pins the shell dialect: on a Windows host `exec_command` and
    /// `shell_command` default to PowerShell, which has no heredoc, so a
    /// nested `apply_patch <<EOF` is only recognised when the call declares
    /// bash (`exec_command` takes a `shell` field; Git-Bash on Windows).
    fn nested(home: &Path, tool: &str, mut input: serde_json::Value) -> Option<String> {
        if tool == "exec_command" && input.get("shell").is_none() {
            input["shell"] = "bash".into();
        }
        let mut payload = patch_payload(home, input);
        payload.tool_name = tool.into();
        payload.command = payload
            .tool_input
            .as_ref()
            .and_then(|v| v.get("cmd").or_else(|| v.get("command")))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .into();
        reason_for_payload_in(&payload, Some(home.to_path_buf()))
    }

    #[test]
    fn windows_cd_backslashes_survive_the_posix_fallback() {
        assert_eq!(
            literal_backslash_cd("cd C:\\a\\b && apply_patch <<'EOF'\nx\\y\nEOF"),
            "cd C:\\\\a\\\\b && apply_patch <<'EOF'\nx\\y\nEOF"
        );
        for unchanged in [
            "cd \"C:\\a\" && x",
            "apply_patch <<'EOF'\nEOF",
            "cd /a && x",
        ] {
            assert_eq!(literal_backslash_cd(unchanged), unchanged);
        }
    }

    #[test]
    fn non_posix_dialect_heredoc_patches_are_judged_by_their_targets() {
        let home = tempfile::tempdir().unwrap();
        let settings = crate::clud_settings::settings_path_at(home.path());
        let name = settings_name(home.path());
        let source = format!(
            "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n\
             -// reads ~/.clud/{name}\n+// ok\n*** End Patch"
        );
        let ordinary = format!("apply_patch <<'EOF'\n{source}\nEOF");
        let hit = format!(
            "apply_patch <<'EOF'\n*** Begin Patch\n*** Update File: {}\n@@\n-a\n+b\n*** End Patch\nEOF",
            settings.display()
        );
        for dialect in [
            crate::block_bad_cmd::ShellDialect::PowerShell,
            crate::block_bad_cmd::ShellDialect::Cmd,
        ] {
            let judge = |command: &str| {
                nested_patch_reason(command, dialect, home.path(), Some(home.path()))
            };
            assert_eq!(judge(&ordinary), Some(None), "{dialect:?}");
            assert!(judge(&hit).flatten().is_some(), "{dialect:?}");
        }
    }

    #[test]
    fn nested_exec_command_patches_are_judged_by_their_targets() {
        let home = tempfile::tempdir().unwrap();
        let settings = crate::clud_settings::settings_path_at(home.path());
        // Source text that merely discusses the protected file is ordinary.
        let source = "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n\
            -// reads ~/.clud/settings.json and extra_roots\n+// ok\n*** End Patch";
        let ordinary = format!("apply_patch <<'EOF'\n{source}\nEOF");
        // Single-quoted so a Windows `C:\...` path keeps its backslashes.
        let with_cd = format!(
            "cd '{}' && apply_patch <<'EOF'\n{source}\nEOF",
            home.path().display()
        );
        for (tool, key, command) in [
            ("exec_command", "cmd", &ordinary),
            ("exec_command", "cmd", &with_cd),
            ("apply_patch", "command", &ordinary),
        ] {
            let input = serde_json::json!({ key: command });
            assert_eq!(nested(home.path(), tool, input), None, "{tool} {command}");
        }
        let hit = format!(
            "apply_patch <<'EOF'\n*** Begin Patch\n*** Update File: {}\n@@\n-a\n+b\n*** End Patch\nEOF",
            settings.display()
        );
        assert_protected(nested(
            home.path(),
            "exec_command",
            serde_json::json!({"cmd": hit}),
        ));
        let headerless = "apply_patch <<'EOF'\n@@\n+x\nEOF";
        assert_eq!(
            nested(
                home.path(),
                "exec_command",
                serde_json::json!({"cmd": headerless})
            )
            .as_deref(),
            Some(NO_TARGETS)
        );
        // A relative target resolves against the `cd` directory, not the payload cwd.
        let relative = format!(
            "cd '{}' && apply_patch <<'EOF'\n*** Begin Patch\n*** Update File: settings.json\n@@\n-a\n+b\n*** End Patch\nEOF",
            settings.parent().unwrap().display()
        );
        assert_protected(nested(
            home.path(),
            "exec_command",
            serde_json::json!({"cmd": relative}),
        ));
    }

    /// `shell_command` has no `shell` field, so its dialect is the host's:
    /// PowerShell on Windows (no heredoc, so the nested patch is unparseable
    /// and fails closed to the general rule), POSIX elsewhere.
    #[test]
    fn nested_shell_command_patch_follows_the_host_dialect() {
        let home = tempfile::tempdir().unwrap();
        let source = "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n\
            -// reads ~/.clud/settings.json\n+// ok\n*** End Patch";
        let command = format!("apply_patch <<'EOF'\n{source}\nEOF");
        let got = nested(
            home.path(),
            "shell_command",
            serde_json::json!({"command": command}),
        );
        // Host-independent simulation of the Windows default: PowerShell has
        // no heredoc, so the POSIX fallback judges the patch by its targets
        // (#2378) and the result is the same on every host.
        assert_eq!(
            nested_patch_reason(
                &command,
                crate::block_bad_cmd::ShellDialect::PowerShell,
                home.path(),
                Some(home.path())
            ),
            Some(None)
        );
        assert_eq!(got, None);
    }

    #[test]
    fn nested_patch_after_unreliable_cd_connectors_is_not_trusted() {
        let home = tempfile::tempdir().unwrap();
        let body = "<<'EOF'\n*** Begin Patch\n*** Update File: ../.clud/settings.json\n@@\n-a\n+b\n*** End Patch\nEOF";
        let cwd = home.path().join("proj");
        for command in [
            format!("cd /nonexistent_zz; apply_patch {body}"),
            format!("cd /nonexistent_zz\napply_patch {body}"),
            format!("cd /nonexistent_zz || apply_patch {body}"),
            format!("cd /a && cd /b && apply_patch {body}"),
            format!("apply_patch {body} && cd /a"),
        ] {
            let mut payload = patch_payload(home.path(), serde_json::json!({}));
            payload.tool_name = "exec_command".into();
            payload.cwd = cwd.clone();
            payload.command = command.clone();
            let dialect = super::super::shell_dialect_for_payload(&payload);
            assert_eq!(
                nested_patch_reason(&command, dialect, &cwd, Some(home.path())),
                None,
                "{command}"
            );
            // The general fail-closed rule then judges it.
            let reaching = command.replace("../.clud", "~/.clud");
            payload.command = reaching.clone();
            assert!(
                reason_for_payload_in(&payload, Some(home.path().to_path_buf())).is_some(),
                "{reaching}"
            );
        }
        let ok = format!(
            "cd '{}' && apply_patch <<'EOF'\n*** Begin Patch\n*** Update File: x.rs\n@@\n-a\n+b\n*** End Patch\nEOF",
            cwd.display()
        );
        assert_eq!(
            nested(home.path(), "exec_command", serde_json::json!({"cmd": ok})),
            None
        );
    }

    #[test]
    fn cdpath_immunity_follows_platform_absolute_paths() {
        for t in [
            "/x", "./x", "../x", ".", "..", "C:/x", "C:\\x", ".\\x", "..\\x",
        ] {
            assert!(target_ignores_cdpath(t), "{t}");
        }
        assert!(target_ignores_cdpath("\\\\srv\\share"));
        for t in ["x", ".clud", "x/y", "C:x"] {
            assert!(!target_ignores_cdpath(t), "{t}");
        }
        assert_eq!(
            Path::new("C:\\x").is_absolute(),
            crate::platform::host::WINDOWS
        );
    }

    #[test]
    fn nested_cd_is_trusted_only_when_cdpath_cannot_apply() {
        let home = tempfile::tempdir().unwrap();
        let cwd = home.path().join("proj");
        let body = "<<'EOF'\n*** Begin Patch\n*** Update File: settings.json\n@@\n-a\n+b\n*** End Patch\nEOF";
        for command in [
            format!("CDPATH=x cd .clud && apply_patch {body}"),
            format!("CDPATH=x cd ./.clud && apply_patch {body}"),
            format!("cd .clud && apply_patch {body}"),
        ] {
            assert_eq!(
                nested_patch_reason(
                    &command,
                    super::super::ShellDialect::Posix,
                    &cwd,
                    Some(home.path())
                ),
                None,
                "{command}"
            );
        }
        let settings = crate::clud_settings::settings_path_at(home.path());
        let name = settings.file_name().unwrap().to_string_lossy().into_owned();
        let dir = settings
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let protected = format!(
            "cd ./{dir} && apply_patch <<'EOF'\n*** Begin Patch\n*** Update File: {name}\n@@\n-a\n+b\n*** End Patch\nEOF"
        );
        let reason = nested_patch_reason(
            &protected,
            super::super::ShellDialect::Posix,
            home.path(),
            Some(home.path()),
        );
        assert!(
            reason
                .flatten()
                .is_some_and(|r| r.contains("[protected-target]")),
            "{protected}"
        );
        let abs = format!(
            "cd '{}' && apply_patch <<'EOF'\n*** Begin Patch\n*** Update File: x.rs\n@@\n-a\n+b\n*** End Patch\nEOF",
            cwd.display()
        );
        assert_eq!(
            nested(home.path(), "exec_command", serde_json::json!({"cmd": abs})),
            None
        );
    }

    #[test]
    fn direct_apply_patch_reaches_the_gate_through_the_entry_point() {
        let home = tempfile::tempdir().unwrap();
        let payload = patch_payload(home.path(), serde_json::json!({"command": ORDINARY}));
        assert_eq!(
            reason_for_payload_in(&payload, Some(home.path().to_path_buf())),
            None
        );
        let garbage = patch_payload(home.path(), serde_json::json!(7));
        assert_eq!(
            reason_for_payload_in(&garbage, Some(home.path().to_path_buf())).as_deref(),
            Some(UNRECOGNIZED_PAYLOAD)
        );
    }

    #[test]
    fn unresolved_home_is_reported_as_such() {
        let cwd = tempfile::tempdir().unwrap();
        let payload = patch_payload(cwd.path(), serde_json::json!(ORDINARY));
        assert_eq!(
            native_file_reason(&payload, "apply_patch", None).as_deref(),
            Some(NO_HOME)
        );
    }

    fn reason(command: &str) -> Option<String> {
        reason_in(
            command,
            crate::block_bad_cmd::ShellDialect::Posix,
            Some(Path::new("/repo")),
            Some(Path::new("/home/u")),
        )
    }

    #[test]
    fn commands_that_write_the_settings_file_are_refused() {
        for command in [
            "echo '{}' > ~/.clud/settings.json",
            "printf x >> $HOME/.clud/settings.json",
            "jq '.safe_rm.extra_roots=[{\"path\":\"/srv\"}]' ~/.clud/settings.json > /x/s && mv /x/s ~/.clud/settings.json",
            "cat new.json | tee ~/.clud/settings.json",
            "sed -i 's/a/b/' ~/.clud/settings.json",
            "sed -n 'w /home/u/.clud/settings.json' notes.txt",
            "python -c \"open('/home/u/.clud/settings.json','w').write('{}')\"",
            "cp evil.json ~/.clud/settings.json",
            "cd ~/.clud && echo x > settings.json",
            "perl -pi -e 's/x/y/' \"$HOME/.clud/settings.json\"",
            "node -e 'require(\"fs\").writeFileSync(process.env.HOME+\"/.clud/settings.json\",\"{}\")'",
            "dd if=x of=~/.clud/settings.json",
            "clud settings set safe_rm.extra_roots /srv",
            "CLUD_ALLOW_ALL_CMDS=1 cp x ~/.clud/settings.json",
            "echo '{\"safe_rm\":{\"extra_roots\":[]}}' > /home/u/.clud/settings.json",
        ] {
            let got = reason(command);
            assert!(got.is_some(), "{command}");
            assert!(got.unwrap().contains("safe_rm.extra_roots"), "{command}");
        }
    }

    #[test]
    fn reads_and_unrelated_commands_are_allowed() {
        for command in [
            "cat ~/.clud/settings.json",
            "jq .safe_rm ~/.clud/settings.json",
            "grep extra_roots ~/.clud/settings.json 2>/dev/null",
            "clud settings --list",
            "safe-rm -r /srv/out",
            "echo settings.json > notes.txt",
            "cargo test",
            "cat ./settings.json",
            "gh issue list --repo zackees/clud --search 'safe_rm.extra_roots ~/.clud/settings.json'",
            "cat > issue-draft.md <<'EOF'\nDocument ~/.clud/settings.json and safe_rm.extra_roots.\nEOF",
            "sed -n '1,175p' crates/clud-bin/src/rm_tool_extra_roots.rs",
            // #2454: history reads and forge comment text are not writes.
            "git -C ~/dev/x log -1 --format='%h %cr' -- ~/.clud/settings.json",
            "git diff HEAD~1 -- ~/.clud/settings.json",
            "gh issue reopen 2454 --repo zackees/clud --comment 'refused ~/.clud/settings.json'",
            "gh pr comment 9 --body 'edit ~/.clud/settings.json by hand'",
            // The repo layer is never read for the override.
            "jq '.bash.block_cd=false' .clud/settings.json > t && mv t .clud/settings.json",
        ] {
            assert_eq!(reason(command), None, "{command}");
        }
    }

    #[test]
    fn scrubbed_operands_cannot_hide_a_later_write() {
        let n = "settings.json";
        for command in [
            format!("cat a >/dev/null; python -c 'import sys;open(sys.argv[1],\"w\")' \"$_\" ~/.clud/{n}"),
            format!("cat ~/.clud/{n} >/dev/null; python -c 'import sys;open(sys.argv[1],\"w\")' \"$_\""),
            format!("cat ~/.clud/{n} >/dev/null; sed -i s/a/b/ \"$_\""),
            format!("(cat ~/.clud/{n}; sed -i x \"$_\") | cat"),
            format!("(cat ~/.clud/{n} && sed -i x \"$_\") | cat"),
            format!("{{ cat ~/.clud/{n}; sed -i x \"$_\"; }} | cat"),
            format!("cat ~/.clud/{n} > /dev/null; perl -pi -e s/a/b/ \"$_\""),
            format!("python -c \"open(__import__('os').path.expanduser('~/.clud/{n}'),'w')\"; cat clud/{n}"),
            format!("python -c \"open(__import__('os').path.expanduser('~/.clud/{n}'),'w')\""),
            format!("tee ~/.clud/{n} <<'EOF'\nx\nEOF"),
            format!("cat <<EOF > ~/.clud/{n}\nx\nEOF"),
            format!("touch ~/.clud/{n}"),
            format!("git log --output=~/.clud/{n}"),
            format!("git show HEAD:x > ~/.clud/{n}"),
            format!("git checkout -- ~/.clud/{n}"),
            format!("mkdir -p ~/.clud && touch ~/.clud/{n}"),
            format!("less -o ~/.clud/{n} /tmp/x"),
            format!("less -O ~/.clud/{n} /tmp/x"),
            format!("less --log-file=~/.clud/{n} /tmp/x"),
            format!("head -c 9 /tmp/x > ~/.clud/{n}"),
            format!("jq . /tmp/x > ~/.clud/{n}"),
            format!("cat <<-'EOF' > ~/.clud/{n}\n\tx\n\tEOF"),
            format!("cat <<< x > ~/.clud/{n}"),
            format!("cat \"$(cp x ~/.clud/{n})\""),
            format!("cat <(cp x ~/.clud/{n})"),
            format!("cat a; cp b ~/.clud/{n}"),
            format!("tee x.md <<EOF\n$(python -c \"open('~/.clud/{n}','w')\")\nEOF"),
            format!("tee x.md <<'EOF'\n~/.clud/{n}\nEOF\npython -c 'import sys' x.md"),
            format!("cd ~/.clud && tee {n} <<'EOF'\nx\nEOF"),
        ] {
            assert!(reason(&command).is_some(), "{command}");
        }
    }

    #[test]
    fn read_only_scripts_that_name_the_file_stay_allowed() {
        let n = "settings.json";
        for command in [
            format!("cat .clud/{n} 2>/dev/null"),
            format!("cat <<E\\OF\nsee ~/.clud/{n}\nEOF"),
            format!("cat > scratchpad/x.md <<'EOF'\nsee ~/.clud/{n}\nEOF"),
            format!("mkdir -p out && cat ~/.clud/{n} 2>/dev/null"),
            "less -N notes.txt".to_string(),
        ] {
            assert_eq!(reason(&command), None, "{command}");
        }
    }
}

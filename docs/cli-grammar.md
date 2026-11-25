# clud CLI grammar

<!-- GENERATED FILE: do not edit. Source: crates/clud-bin/src/cli_grammar.rs.
Regenerate: CLUD_REGEN_CLI_GRAMMAR=1 soldr cargo test -p clud --lib cli_grammar -->

The complete command-line grammar of `clud`, rendered from the clap
definitions in `crates/clud-bin/src/args.rs` (and the subcommand
parsers it embeds), the passthrough splitter's tables, and the
argv[0] dispatch table in `crates/clud-bin/src/multicall.rs`. A drift
test fails CI when this file and the code disagree.

`clud --help` and `clud <command> --help` print the same facts for one
command. Why the launch pipeline is shaped this way lives in
[launch-plan.md](architecture/launch-plan.md) and
[provider-selection.md](architecture/provider-selection.md).

Contents: [argv pipeline](#argv-pipeline) · [grammar](#grammar-ebnf) ·
[passthrough](#passthrough-rules) · [multicall names](#multicall-names) ·
[environment](#environment-variables) · [exit codes](#exit-codes) ·
[commands](#commands)

## argv pipeline

Every invocation passes these stages in order (`main.rs`, `args.rs`):

1. **argv[0] dispatch.** If the executable was invoked under a helper name
(see [multicall names](#multicall-names)), that function runs and nothing
below applies.
2. **Normalization**, in the top-level option region only (before the first
subcommand or `--`): a numeric value after `-c`/`--continue` becomes the
session index (`clud -c 2`); a bare `--resume`/`-r` followed by a
subcommand or flag means "open the picker" (use `--resume=<session>` for
a session named like a verb); a flag glued to its value by a non-ASCII
space is split; a public long option spelled with a Unicode dash (or
`-deepseek`) is corrected to `--`; `--deepseek=<KEY>`, `--kimi=<KEY>` and
`--openrouter=<KEY>` split into flag and key, and any non-key value is an
error.
3. **Splitting** (`split_known_unknown`): tokens clud owns go to clap; every
other token is forwarded to the backend harness (see
[passthrough rules](#passthrough-rules)). A flag-shaped near miss of a
clud option is rejected with a suggestion instead of being forwarded.
4. **Placement check.** A root-only flag written after a subcommand
(`clud do --model x`) is rejected with the reordered command line.
5. **clap parse** of the owned tokens against the tables under
[commands](#commands). For `safe-rm`, every token after the verb is
handed to its rm-style parser verbatim.

## Grammar (EBNF)

ISO 14977 EBNF: `,` concatenation, `|` alternation, `[ ]` optional,
`{ }` zero or more. `OPTION` is any flag in the enclosing command's
table under [commands](#commands), in any order and interleaved with
positionals; an upper-case name is one argv token. Hidden internal
commands are included because they parse.

```ebnf
clud = "clud" , { OPTION } , [ clud-ab | clud-remote | clud-clud-bot | clud-connect | clud-gate | clud-serve | clud-secret | clud-sudo | clud-secrets-mcp | clud-secrets-daemon | clud-computer-use | clud-run | clud-models | clud-codex-update | clud-dsh-update | clud-setup | clud-install | clud-pr-check | clud-doctor | clud-inject | clud-ci | clud-auth | clud-codex-auth | clud-deepseek-auth | clud-loop | clud-up | clud-rebase | clud-fix | clud-do | clud-grind | clud-video | clud-do-prompt | clud-grind-scripts | clud-safe-rm | clud-grind-facts | clud-install-assets | clud-workflow | clud-wasm | clud-attach | clud-kill | clud-slay | clud-list | clud-claim | clud-top | clud-logs | clud-gc | clud-config | clud-extern | clud-ui | clud-trash | clud-tool | clud-log | clud-optimize | clud-daemon | clud-symbols | clud-test | clud-settings | clud-statusline | clud-session | clud-session-hook | clud-__start-cmd | clud-__openai-chat-translate | clud-__gemini-client-translate | clud-__claude-gemini-translate | clud-__apply-payload-rules | clud-__openai-completions-translate | clud-__openai-responses-translate | clud-__xai-device-login | clud-__kimi-device-login | clud-__meta-device-login | clud-__daemon | clud-__worker ] , [ "--" , { BACKEND-ARGS } ] ;
clud-ab = "ab" , { OPTION } ;
clud-remote = "remote" , { OPTION } , ( clud-remote-connect ) ;
clud-remote-connect = "connect" , { OPTION } , [ HOST ] ;
clud-clud-bot = "clud.bot" , { OPTION } ;
clud-connect = "connect" , { OPTION } , [ HOST ] ;
clud-gate = "gate" , { OPTION } , LANE ;
clud-serve = "serve" , { OPTION } ;
clud-secret = "secret" , { OPTION } ;
clud-sudo = "sudo" , { OPTION } , COMMAND , { COMMAND } ;
clud-secrets-mcp = "secrets-mcp" , { OPTION } ;
clud-secrets-daemon = "secrets-daemon" , { OPTION } ;
clud-computer-use = "computer-use" , { OPTION } , ( clud-computer-use-status | clud-computer-use-install | clud-computer-use-doctor | clud-computer-use-grant | clud-computer-use-mcp ) ;
clud-computer-use-status = "status" , { OPTION } ;
clud-computer-use-install = "install" , { OPTION } ;
clud-computer-use-doctor = "doctor" , { OPTION } ;
clud-computer-use-grant = "grant" , { OPTION } ;
clud-computer-use-mcp = "mcp" , { OPTION } ;
clud-run = "run" , { OPTION } ;
clud-models = "models" , { OPTION } , ( clud-models-cheapest ) ;
clud-models-cheapest = "cheapest" , { OPTION } ;
clud-codex-update = "codex-update" , { OPTION } ;
clud-dsh-update = "dsh-update" , { OPTION } ;
clud-setup = "setup" , { OPTION } , [ PRODUCT ] ;
clud-install = "install" , { OPTION } , PRODUCT ;
clud-pr-check = "pr-check" , { OPTION } ;
clud-doctor = "doctor" , { OPTION } ;
clud-inject = "inject" , { OPTION } , SESSION , MESSAGE ;
clud-ci = "ci" , { OPTION } , [ JOB ] ;
clud-auth = "auth" , { OPTION } , [ clud-auth-login | clud-auth-status | clud-auth-logout ] ;
clud-auth-login = "login" , { OPTION } , PROVIDER ;
clud-auth-status = "status" , { OPTION } , [ PROVIDER ] ;
clud-auth-logout = "logout" , { OPTION } , PROVIDER ;
clud-codex-auth = "codex-auth" , { OPTION } , ( clud-codex-auth-login | clud-codex-auth-status | clud-codex-auth-logout ) ;
clud-codex-auth-login = "login" , { OPTION } ;
clud-codex-auth-status = "status" , { OPTION } ;
clud-codex-auth-logout = "logout" , { OPTION } ;
clud-deepseek-auth = "deepseek-auth" , { OPTION } , ( clud-deepseek-auth-login | clud-deepseek-auth-status | clud-deepseek-auth-logout ) ;
clud-deepseek-auth-login = "login" , { OPTION } ;
clud-deepseek-auth-status = "status" , { OPTION } ;
clud-deepseek-auth-logout = "logout" , { OPTION } ;
clud-loop = "loop" , { OPTION } , [ TASK ] ;
clud-up = "up" , { OPTION } ;
clud-rebase = "rebase" , { OPTION } ;
clud-fix = "fix" , { OPTION } , [ URL ] ;
clud-do = "do" , { OPTION } , [ TARGET ] ;
clud-grind = "grind" , { OPTION } , [ URL ] ;
clud-video = "video" , { OPTION } , [ DIR ] ;
clud-do-prompt = "do-prompt" , { OPTION } , { TARGET } ;
clud-grind-scripts = "grind-scripts" , { OPTION } ;
clud-safe-rm = "safe-rm" , { OPTION } , { ARGS } ;
clud-grind-facts = "grind-facts" , { OPTION } , { ARGS } ;
clud-install-assets = "install-assets" , { OPTION } ;
clud-workflow = "workflow" , { OPTION } , SCRIPT ;
clud-wasm = "wasm" , { OPTION } , MODULE ;
clud-attach = "attach" , { OPTION } , [ SESSION_ID ] ;
clud-kill = "kill" , { OPTION } , [ SESSION_ID ] ;
clud-slay = "slay" , { OPTION } ;
clud-list = "list" , { OPTION } ;
clud-claim = "claim" , { OPTION } , ( clud-claim-acquire | clud-claim-release-own | clud-claim-who | clud-claim-release ) ;
clud-claim-acquire = "acquire" , { OPTION } ;
clud-claim-release-own = "release-own" , { OPTION } ;
clud-claim-who = "who" , { OPTION } ;
clud-claim-release = "release" , { OPTION } , CHECKOUT ;
clud-top = "top" , { OPTION } ;
clud-logs = "logs" , { OPTION } , [ SESSION_ID ] ;
clud-gc = "gc" , { OPTION } , [ clud-gc-list | clud-gc-prune | clud-gc-purge | clud-gc-all | clud-gc-reconcile | clud-gc-keep | clud-gc-branches ] ;
clud-gc-list = "list" , { OPTION } ;
clud-gc-prune = "prune" , { OPTION } , [ KIND ] ;
clud-gc-purge = "purge" , { OPTION } , [ KIND ] ;
clud-gc-all = "all" , { OPTION } ;
clud-gc-reconcile = "reconcile" , { OPTION } ;
clud-gc-keep = "keep" , { OPTION } , [ PATH ] ;
clud-gc-branches = "branches" , { OPTION } ;
clud-config = "config" , { OPTION } , [ clud-config-set | clud-config-show | clud-config-edit ] ;
clud-config-set = "set" , { OPTION } , ASSIGNMENT ;
clud-config-show = "show" , { OPTION } ;
clud-config-edit = "edit" , { OPTION } ;
clud-extern = "extern" , { OPTION } , [ clud-extern-trust ] ;
clud-extern-trust = "trust" , { OPTION } , [ NAME ] ;
clud-ui = "ui" , { OPTION } ;
clud-trash = "trash" , { OPTION } , PATH , { PATH } ;
clud-tool = "tool" , { OPTION } , ( clud-tool-run | clud-tool-list | clud-tool-log | clud-tool-ledger | clud-tool-info ) ;
clud-tool-run = "run" , { OPTION } , REL_PATH , { ARGS } ;
clud-tool-list = "list" , { OPTION } ;
clud-tool-log = "log" , { OPTION } , [ REFERENCE ] ;
clud-tool-ledger = "ledger" , { OPTION } ;
clud-tool-info = "info" , { OPTION } , [ REFERENCE ] ;
clud-log = "log" , { OPTION } ;
clud-optimize = "optimize" , { OPTION } , [ TARGET ] ;
clud-daemon = "daemon" , { OPTION } , ( clud-daemon-api-info | clud-daemon-restart | clud-daemon-stop | clud-daemon-running-process | clud-daemon-orphan-status ) ;
clud-daemon-api-info = "api-info" , { OPTION } ;
clud-daemon-restart = "restart" , { OPTION } ;
clud-daemon-stop = "stop" , { OPTION } ;
clud-daemon-running-process = ( "running-process" | "servicedef" ) , { OPTION } ;
clud-daemon-orphan-status = "orphan-status" , { OPTION } ;
clud-symbols = "symbols" , { OPTION } , [ clud-symbols-install | clud-symbols-verify ] ;
clud-symbols-install = "install" , { OPTION } ;
clud-symbols-verify = "verify" , { OPTION } ;
clud-test = "test" , { OPTION } , ( clud-test-run | clud-test-capture | clud-test-stats ) ;
clud-test-run = "run" , { OPTION } , { COMMAND } ;
clud-test-capture = "capture" , { OPTION } , { COMMAND } ;
clud-test-stats = "stats" , { OPTION } ;
clud-settings = "settings" , { OPTION } , [ clud-settings-set | clud-settings-roots ] ;
clud-settings-set = "set" , { OPTION } , POINTER , VALUE ;
clud-settings-roots = "roots" , { OPTION } , VALUE ;
clud-statusline = "statusline" , { OPTION } ;
clud-session = "session" , { OPTION } , ( clud-session-stats | clud-session-new | clud-session-show ) ;
clud-session-stats = "stats" , { OPTION } ;
clud-session-new = "new" , { OPTION } ;
clud-session-show = "show" , { OPTION } , ID ;
clud-session-hook = "session-hook" , { OPTION } ;
clud-__start-cmd = "__start-cmd" , { OPTION } ;
clud-__openai-chat-translate = "__openai-chat-translate" , { OPTION } ;
clud-__gemini-client-translate = "__gemini-client-translate" , { OPTION } ;
clud-__claude-gemini-translate = "__claude-gemini-translate" , { OPTION } ;
clud-__apply-payload-rules = "__apply-payload-rules" , { OPTION } ;
clud-__openai-completions-translate = "__openai-completions-translate" , { OPTION } ;
clud-__openai-responses-translate = "__openai-responses-translate" , { OPTION } ;
clud-__xai-device-login = "__xai-device-login" , { OPTION } ;
clud-__kimi-device-login = "__kimi-device-login" , { OPTION } ;
clud-__meta-device-login = "__meta-device-login" , { OPTION } ;
clud-__daemon = "__daemon" , { OPTION } ;
clud-__worker = "__worker" , { OPTION } ;
OPTION = ? a flag from the enclosing command's flag table ? ;
BACKEND-ARGS = ? any token; forwarded verbatim to the harness ? ;
```

## Passthrough rules

clud is a launcher: an argv token it does not own is forwarded to the
backend harness (Claude Code, Codex, ...) unchanged, after clud's own
arguments. The splitter decides ownership token by token, left to right:

1. `--` ends clud's flags: everything after it is forwarded verbatim,
except inside a subcommand that owns `--` (below).
2. The first top-level command word switches to that command; from then
on every token belongs to the command's own parser.
3. A known switch, a known value flag plus its value, or a known
`--flag=value` stays with clud.
4. Any other token is forwarded. A flag-shaped token that misspells a
clud option, or names an option the splitter forgot, is an error.

- **Top-level command words:** `ab`, `remote`, `clud.bot`, `connect`, `gate`, `serve`, `do-prompt`, `grind-scripts`, `grind-facts`, `safe-rm`, `install-assets`, `loop`, `up`, `rebase`, `fix`, `do`, `grind`, `video`, `wasm`, `workflow`, `attach`, `kill`, `slay`, `list`, `claim`, `top`, `logs`, `log`, `gc`, `config`, `extern`, `ui`, `trash`, `tool`, `optimize`, `daemon`, `inject`, `symbols`, `settings`, `test`, `auth`, `secret`, `sudo`, `secrets-mcp`, `secrets-daemon`, `computer-use`, `models`, `codex-update`, `dsh-update`, `doctor`, `pr-check`, `setup`, `install`, `ci`, `codex-auth`, `deepseek-auth`, `run`, `statusline`, `session`, `session-hook`, `__daemon`, `__worker`, `__start-cmd`, `__openai-chat-translate`, `__openai-completions-translate`, `__gemini-client-translate`, `__claude-gemini-translate`, `__apply-payload-rules`, `__openai-responses-translate`, `__kimi-device-login`, `__xai-device-login`, `__meta-device-login`
- **Commands that own `--` (their raw argv follows it):** `tool`, `test`, `safe-rm`, `secret`, `sudo`, `grind-facts`
- **Value flags (consume the next token):** `--backends`, `--prompt-file`, `--runs`, `--model-map`, `--shell`, `--repo`, `--install-version`, `--prompt`, `--message`, `--resume`, `--resume-mode`, `--model`, `--allow-model`, `--provider-only`, `--provider`, `--mode`, `--failover`, `--effort`, `--context-window`, `--harness`, `--name`, `--transcript`, `--backlog-size`, `--graphics`, `--graphics-image`, `--loop-count`, `--min-iterations`, `--done`, `--repeat`, `--args`, `--config`, `--daemon-state-dir`, `--stale-after`, `--older-than`, `--daemon`, `--state-dir`, `--cmd`, `--set-web-term`, `--set-openrouter-zdr`, `--reason`, `--session-pid`, `--chain-b64`
- **Short value flags:** `-p`, `-m`, `-r`
- **Switches:** `--no-fast-fail`, `--reuse`, `--zdr`, `--no-data-collection`, `--redact-secrets`, `--no-redact-secrets`, `--installer`, `--installer-json`, `--installer-plan-json`, `--installer-app`, `--install-current`, `--no-modify-path`, `--continue`, `--claude`, `--codex`, `--deepseek`, `--kimi`, `--openrouter`, `--unified`, `--failover-allow-metered`, `--subprocess`, `--pty`, `--safe`, `--no-land`, `--unsafe`, `--unattended`, `--allow-plan-mode`, `--artifact-tool`, `--product-tools`, `--builtin-subagents`, `--allow-secrets`, `--computer-use`, `--no-computer-use`, `--coauthor`, `--dry-run`, `--detach`, `--detachable`, `--verbose`, `--no-cpu-banner`, `--experimental-daemon-centralized`, `--all`, `--last`, `--refresh`, `--no-done`, `--no-done-marker`, `--no-dnd`, `--no-drag-drop`, `--clean-worktrees`, `--fix-hooks`, `--no-fix-hooks`, `--yes`, `--force`, `--no-daemon`, `--keep-orphans`, `--quiet-orphans`, `--explain-orphans`, `--json`, `--fix`, `--no-open`, `--demo-gfx-sixel`, `--help`, `--version`, `--web-term`, `--kitty-term`, `--fail-on-no-server`, `--list`, `--revoke`
- **Short switches:** `-c`, `-v`, `-h`, `-V`, `-y`
- **Switches with an optional `=`-joined value:** `--coauthor`
- **Provider switches that accept an inline API key** (`--flag <KEY>` or `--flag=<KEY>`, keys start with `sk-`): `--deepseek`, `--kimi`, `--openrouter`

## Multicall names

`clud` is the only shipped executable. Invoked under one of these
names (a hardlink, symlink or copy; case-, `.exe`- and
separator-insensitive), it runs that function before any parsing
above. Each name's own argv is that tool's, not clud's.

| argv[0] | Function |
|---|---|
| `clud-cmd-scan` | PreToolUse command scanner (`block_bad_cmd`) |
| `clud-block-bad-cmd` | PreToolUse command scanner (`block_bad_cmd`) |
| `clud-shim` | session shim: interpreter, `gh`, `git` and `rm` aliases |
| `clud-secret` | masked credential prompt (`clud secret`) |
| `clud-askpass` | `GIT_ASKPASS` / `SSH_ASKPASS` helper |
| `clud-remote-shell` | remote-assistance PTY session leader |
| `python` | session shim: interpreter, `gh`, `git` and `rm` aliases |
| `python3` | session shim: interpreter, `gh`, `git` and `rm` aliases | <!-- python-name-lint: allow -->
| `gh` | session shim: interpreter, `gh`, `git` and `rm` aliases |
| `git` | session shim: interpreter, `gh`, `git` and `rm` aliases |
| `rm` | session shim: interpreter, `gh`, `git` and `rm` aliases |
| `safe-rm` | session shim: interpreter, `gh`, `git` and `rm` aliases |
| `safe-mktemp` | session shim: interpreter, `gh`, `git` and `rm` aliases |
| `clud-installer` | native installer |
| `clud-install` | native installer |
| `clud-setup` | native installer |

Installer names also match with a browser download suffix
(`clud-installer (1)`). Three hidden argv[1] forms reach the same entry
points without an alias: `clud __cmd-scan [ARGS]`, `clud __shim <ALIAS> [ARGS]` and
`clud __link-aliases <DIR>`.

## Environment variables

clap reads no environment variable: argv alone decides the parse. These
variables, named in the help of the listed command or flag, change what
a parsed command does.

| Variable | Named by |
|---|---|
| `CLAUDE_CODE_SESSION_ID` | `clud session stats` `--session` |
| `CLUD_BACKLOG_BYTES` | `clud` `--backlog-size` |
| `CLUD_COAUTHOR` | `clud` `--coauthor` |
| `CLUD_DAEMON_HTTP_SERVER` | `clud log`, `clud log` `--fail-on-no-server` |
| `CLUD_PAYLOAD_RULES` | `clud __apply-payload-rules` `--rules`, `clud __apply-payload-rules` `--settings-file` |

## Exit codes

- `0`: success, and `--help` / `--version`.
- `2`: a usage error (unknown subcommand, bad value, conflicting flags,
misplaced global flag, invalid inline key) reported before anything runs.
- A backend launch exits with the harness's own status; `clud test run`
returns its child's exit code unchanged.

Commands that document their own statuses:

| Command | Statement |
|---|---|
| `clud setup` | Exits 0 on success, 2 for a refused request (no terminal without --yes, missing OpenRouter key), 1 on a failed step |
| `clud doctor` | Exits 1 when anything is wrong |
| `clud grind` | `clud grind reconcile` instead runs the feature-branch reconcile pass (#1393) in the current checkout and exits with its status. |
| `clud tool run` | Exit 0 means the tool itself completed successfully |
| `clud log` | By default missing env / unreachable daemon are silent (exit 0) so a hook caller is never broken; with `--fail-on-no-server` either failure causes a non-zero exit so tests can prove a real round-trip |
| `clud daemon orphan-status` | Exits non-zero if no sweep has run within 2× the sweep interval |
| `clud symbols` | `clud symbols` (bare) prints a summary; `clud symbols install` and `clud symbols verify` exit 1 when any inspected report is unsymbolicated. |
| `clud symbols install` | Exits 1 if any inspected report is unsymbolicated |

## Commands

Every command accepts `-h`/`--help` unless its section says otherwise.
Hidden commands are internal (hooks, tests, the daemon) and are not part
of the supported interface.

### `clud`

Fast CLI for running supported agent harnesses in YOLO mode

```text
clud [OPTIONS] [-- <BACKEND_ARGS>...] [COMMAND]
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `BACKEND_ARGS...` | `<BACKEND_ARGS>` (repeatable) |  | Arguments after `--`, forwarded verbatim to the backend harness. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--web-term` | switch |  | Open this backend launch in clud's owned web terminal window. Conflicts with `--set-web-term`. |
| `--kitty-term` | switch |  | Open this backend launch in the Kitty-compatible desktop terminal. Conflicts with `--web-term`, `--set-web-term`. |
| `--shell` | `bash` |  | Experimental, Windows: run this launch under the named shell in the bundled WezTerm fork. Elsewhere the launch runs normally. |
| `--set-web-term` | `[=on|off]` |  | Persist the web-terminal preference. With no value this enables it; pass `off` to restore the ordinary console launch. Conflicts with `--web-term`. |
| `-p`, `--prompt` | `<PROMPT>` |  | Run with this prompt and exit when the agent finishes. |
| `-m`, `--message` | `<MESSAGE>` |  | Send a one-off message. |
| `-c`, `--continue` | switch |  | Continue a conversation: at a terminal, pick one of this directory's Claude-harness sessions; `-c N` resumes the Nth newest (0 is the last) without the picker. Without a terminal, the harness's own `--continue` |
| `-r`, `--resume` | `[=<RESUME>]` |  | Resume a session, or open the picker when no value is supplied. Use `--resume=<session>` when the session name matches a built-in verb. |
| `--last` | switch |  | Continue the newest Claude-harness session in this directory without opening the `-c` picker (#922) |
| `--resume-mode` | `auto|native|portable` | `auto` | How `-c`/`--last` resumes the selected session: `auto` (native when safe, portable recovery otherwise), `native` (Claude's own resume), or `portable` (a new session rebuilt from the transcript) |
| `--claude` | switch |  | Use Claude as the backend (the default) Conflicts with `--codex`, `--deepseek`, `--kimi`, `--openrouter`. |
| `--codex` | switch |  | Use Codex as the backend. Conflicts with `--claude`, `--deepseek`, `--kimi`, `--openrouter`. |
| `--deepseek` | switch |  | Use DeepSeek's Anthropic-compatible API through the Claude harness. Conflicts with `--claude`, `--codex`, `--kimi`, `--openrouter`. |
| `--kimi` | switch |  | Use Kimi's Anthropic-compatible API through the Claude harness. Conflicts with `--claude`, `--codex`, `--deepseek`, `--openrouter`, `--provider`, `--unified`, `--mode`. |
| `--openrouter` | switch |  | Use OpenRouter's Anthropic-compatible API through the Claude harness. Conflicts with `--claude`, `--codex`, `--deepseek`, `--kimi`, `--provider`, `--unified`, `--mode`. |
| `--provider` | `claude|codex|deepseek|kimi|openrouter` |  | Select a provider using the generic script-friendly spelling. Conflicts with `--claude`, `--codex`, `--deepseek`, `--kimi`, `--openrouter`. |
| `--unified` | switch |  | Route configured providers through one Claude model picker. Conflicts with `--claude`, `--codex`, `--deepseek`, `--kimi`, `--openrouter`, `--provider`, `--mode`. |
| `--mode` | `unified` |  | Generic spelling for the launch routing mode. Conflicts with `--claude`, `--codex`, `--deepseek`, `--kimi`, `--openrouter`, `--provider`, `--unified`. |
| `--failover` | `<ROUTES>` |  | Ordered fallback routes for `--unified`, tried when the active route is exhausted, drained, or rejects its credential. Comma-separated, in descent order, e.g. `--failover claude-opus-4-1,codex-terra` |
| `--failover-allow-metered` | switch |  | Allow descending onto a rung billed per token. Without it, metered rungs are listed but never taken, so automatic recovery cannot become an automatic charge. |
| `--harness` | `default|claude|codex|deepseek|opencode` |  | Select the agent harness independently from the model provider. |
| `--subprocess` | switch |  | Force subprocess launch mode. Conflicts with `--pty`. |
| `--pty` | switch |  | Force PTY launch mode. Conflicts with `--subprocess`. |
| `--graphics` | `auto|off|sixel` | `auto` | Control terminal graphics headers for PTY sessions. |
| `--graphics-image` | `<PATH>` |  | Render this image as the PTY graphics header when Sixel is enabled. |
| `--demo-gfx-sixel` | switch |  | Render the bundled README hero image as a standalone Sixel demo and exit. |
| `--model` | `<MODEL>` |  | Model to launch with (an alias such as `opus`, or a provider model ID) |
| `--allow-model` | `<MODEL>` (repeatable) |  | Constrain every model this launch can reach -- the main model, the haiku/background and subagent slots, the rows gateway discovery may advertise, and anything a bridge will serve -- to an explicit set. Repeatable. Without it, `--model <id>` alone is the allowlist; with neither, nothing is constrained and behavior is unchanged (#1257) |
| `--provider-only` | `<PROVIDER>` (repeatable) |  | OpenRouter only: serve every request from exactly these upstream providers (OpenRouter's `provider.only`, e.g. `parasail/fp8`), with fallbacks off, so a request fails rather than rerouting to another provider or price. Repeatable. |
| `--zdr` | switch |  | OpenRouter only: restrict routing to zero-data-retention endpoints. |
| `--set-openrouter-zdr` | `[=on|off]` |  | Save OpenRouter zero-data-retention routing for future launches and exit. With no value enables it; pass off to disable the saved requirement. Conflicts with `--dry-run`, `--zdr`. |
| `--no-data-collection` | switch |  | OpenRouter only: exclude endpoints that collect request data. |
| `--redact-secrets` | switch |  | Tokenize credentials before provider requests and restore them in responses. Conflicts with `--no-redact-secrets`. |
| `--no-redact-secrets` | switch |  | Disable automatic reversible secret redaction, including free models. Conflicts with `--redact-secrets`. |
| `--effort` | `<EFFORT>` |  | Reasoning effort, kept independent from the selected model. |
| `--context-window` | `<CONTEXT_WINDOW>` |  | Requested context window (for example `1m`) where the selected model supports it. |
| `--no-land` | switch |  | Disable the launch-level preference to finish work through a PR. |
| `--safe` | switch |  | Disable YOLO mode: do not inject the harness's permission-bypass flag. Conflicts with `--unsafe`. |
| `--unsafe` | switch |  | Disable clud's launch-scoped agent safety policy, including deletion redirects and command guards. Explicit safe-rm calls retain their meaning. Conflicts with `--safe`. |
| `--unattended` | switch |  | Strip the harness tools that stall an unattended run waiting on a human: plan mode and multiple-choice questions. `--dangerously-skip- permissions` does not cover these — the model volunteers them on its own, most visibly at the top of each `/loop` iteration, and the run then sits idle until someone approves the plan. Claude harness only; Codex has no equivalent tool surface. |
| `--allow-plan-mode` | switch |  | Allow automatic plan mode during native /loop or scheduled work. Also restore plan mode on the Codex-provider / Claude-harness bridge, where clud otherwise disallows `EnterPlanMode` unconditionally. The bridge suppresses it because the harness hands the model an `EnterPlanMode` tool whose own description tells it to reach for plan mode proactively on any non-trivial implementation ask — so a plain question turns into an unrequested planning session. Suppression is not tied to `--unattended` here: it applies to interactive bridge runs too. `AskUserQuestion` is deliberately left alone; only plan mode is stripped. |
| `--artifact-tool` | switch |  | Keep Claude Code's `Artifact` tool available to the model. clud strips it by default: the tool's description plus input schema is 58.7 KiB of the 122.8 KiB tool payload clud sends on every request, and it is used a handful of times a day. The tool publishes a page to an account, so a forge-cli loop or codex run never needs it. Pass this flag when a session genuinely wants to publish an artifact. |
| `--product-tools` | switch |  | Keep Claude Code's product-surface tools available to the model. clud strips `SendFeedback`, `ReportFindings`, `DesignSync`, `PushNotification` and `RemoteTrigger` from every Claude launch: each one ships as a full tool definition on every request, and over 603 local sessions / 7 days of transcripts the fleet called none of them (the feedback tool: 19 calls). 12.1 KiB of the 122.8 KiB tool payload goes with them. Pass this flag when a session genuinely wants to report feedback, publish a design, or push a notification. |
| `--builtin-subagents` | switch |  | Keep Claude Code's own `general-purpose` subagent definition. clud replaces it by default with a worker definition (Read, Bash, Edit, Write, Skill, ToolSearch); the built-in one is 27,930 B per subagent request, including the `Agent` tool (8,763 B) that 702 subagent sidechains reached for 19 times in 7 days, and `ListAgents` (2,081 B) naming peers that a subagent cannot message. Pass this flag when a subagent should be able to spawn nested subagents. |
| `--allow-secrets` | switch |  | Tell the agent which credentials exist on this machine. Adds the lokr MCP server and its background scanner to the session; values are never returned to the agent. |
| `--no-computer-use` | switch |  | Leave computer use (the `cua` MCP server driving this desktop) out of this launch. It is on by default for the Claude harness. Conflicts with `--computer-use`. |
| `--computer-use` | switch |  | Register computer use on a harness that does not get it by default (Codex, which loads every MCP tool schema, ~220 KB, on each request) |
| `--coauthor` | `[=<TAG>]` |  | Opt in to Claude Code commit/PR attribution (#1317). clud hides the `Co-Authored-By: Claude …` trailer and "Generated with Claude Code" PR line by default; `--coauthor` keeps Claude Code's own, and `--coauthor=TAG` uses TAG for both. `CLUD_COAUTHOR=1\|TAG` does the same. Claude harness only; other harnesses ignore it. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |
| `--detach` | switch |  | Start a daemon-managed session in the background without attaching. Conflicts with `--dry-run`. |
| `--detachable` | switch |  | Run attached under the daemon; Ctrl+C asks whether to keep the session running in the background. Conflicts with `--dry-run`. |
| `--name` | `<SESSION_NAME>` |  | Name the daemon-managed session (shown by `clud list` and `clud top`) |
| `--transcript` | `<PATH>` |  | Write daemon-managed session output bytes to a transcript file. Implies centralized daemon execution. |
| `--backlog-size` | `<BACKLOG_SIZE>` |  | Override the in-memory attach-replay backlog cap. Accepts bytes (`262144`), or SI/binary suffixes (`256k`, `256KiB`, `1mb`). The compiled default is 256 KiB. Also honored as `CLUD_BACKLOG_BYTES` |
| `-v`, `--verbose` | switch |  | Show debug output. |
| `--no-dnd` | switch |  | Disable the Windows console drag-and-drop target registration. Issue #79: by default `clud` registers an `IDropTarget` on the console window so dragged files are forwarded to the backend. Pass `--no-dnd` to opt out (no-op on POSIX, where drops already arrive as bracketed-paste stdin bytes) |
| `--clean-worktrees` | switch |  | Issue #83: enumerate this repo's git worktrees and remove the stale ones. Combine with `--dry-run` to preview, `--yes` to skip confirmation, `--force` to also remove dirty/unpushed worktrees. |
| `--fix-hooks` | switch |  | Inspect Claude/Codex PreToolUse hook parity and apply explicit, repo-scoped repairs where clud can do so safely. |
| `--no-fix-hooks` | switch |  | Globally disable automatic deterministic hook-health repairs on launch. Conflicts with `--fix-hooks`. |
| `--stale-after` | `<STALE_AFTER>` | `1d` | Issue #83: minimum age before a clean worktree is treated as stale. Accepts `30s`, `5m`, `2h`, `1d`. Defaults to `1d` |
| `-y`, `--yes` | switch |  | Issue #83: skip interactive confirmation prompts (combined with `--clean-worktrees`) |
| `--installer` | switch |  | Open the native clud installer selector before ordinary startup. |
| `--installer-json` | switch |  | Emit a structured installer outcome on stdout (explicit choice required) |
| `--installer-app` | switch |  | `Install clud.app`'s helper entry (#2863): run the dialog installer as the bundle's pristine `Contents/Helpers/clud`. Internal. Hidden. |
| `--installer-plan-json` | switch |  | Print the read-only install plan as JSON (`clud-installer-plan/v1`) and exit without writing (explicit choice required) Conflicts with `--installer-json`, `-y`, `--yes`. |
| `--install-current` | switch |  | Select the invoking executable for installation. Conflicts with `--install-version`. |
| `--install-version` | `<VERSION>` |  | Select an exact published clud release for installation. Conflicts with `--install-current`. |
| `--no-modify-path` | switch |  | Install the executable without changing shell profiles or User PATH. |
| `--force` | switch |  | Issue #83: allow `--clean-worktrees` to remove dirty / unpushed worktrees. Locked worktrees are still preserved. |
| `--experimental-daemon-centralized` | switch |  | Hidden. |
| `--daemon-state-dir` | `<DAEMON_STATE_DIR>` |  | Hidden. |
| `--daemon` | `<MODE>` |  | Issue #135: reserved for forward compatibility. The merged always-on clud daemon (`__daemon`) hosts both session ops and the GC registry, so the prior `--daemon=gc` / `--daemon=session` distinction is no longer required. Kept as an accepted flag so older clud invocations don't error. Hidden. |
| `--no-daemon` | switch |  | Issue #135: opt out of the GC daemon auto-spawn for this invocation. `clud gc *` operations fail fast with this flag because they require the daemon. Other code paths skip the spawn silently. |
| `--keep-orphans` | switch |  | Issue #340: keep env-tagged orphaned descendants alive on exit (skip the auto-reap; still prints the report unless --quiet-orphans) |
| `--quiet-orphans` | switch |  | Issue #340: suppress the orphan-reaper surprise report on exit. Reaping still happens unless --keep-orphans is also set. |
| `--explain-orphans` | switch |  | Issue #340: dump the relevant env vars for each detected orphan alongside the report, to help author allowlist rules. |
| `--no-cpu-banner` | switch |  | Issue #466: suppress the foreground CPU-burn banner for this invocation. Banner is otherwise on by default; it polls the subtree CPU every 2 s and emits `[clud] cpu N % …` to stderr when subtree CPU crosses `max(50 %, 0.20 × num_cpus × 100 %)` for 3 sustained ticks. Permanent opt-out via `[foreground.cpu_banner] enabled = false` in `~/.clud/settings.json` |

Subcommands (optional): `ab`, `remote`, `clud.bot`, `connect`, `gate`, `serve`, `secret`, `sudo`, `secrets-mcp`, `secrets-daemon`, `computer-use`, `run`, `models`, `codex-update`, `dsh-update`, `setup`, `install`, `pr-check`, `doctor`, `inject`, `ci`, `auth`, `codex-auth`, `deepseek-auth`, `loop`, `up`, `rebase`, `fix`, `do`, `grind`, `video`, `do-prompt`, `grind-scripts`, `safe-rm`, `grind-facts`, `install-assets`, `workflow`, `wasm`, `attach`, `kill`, `slay`, `list`, `claim`, `top`, `logs`, `gc`, `config`, `extern`, `ui`, `trash`, `tool`, `log`, `optimize`, `daemon`, `symbols`, `test`, `settings`, `statusline`, `session`, `session-hook`, `__start-cmd`, `__openai-chat-translate`, `__gemini-client-translate`, `__claude-gemini-translate`, `__apply-payload-rules`, `__openai-completions-translate`, `__openai-responses-translate`, `__xai-device-login`, `__kimi-device-login`, `__meta-device-login`, `__daemon`, `__worker`.

### `clud ab`

Compare backend latency with isolated, validated samples

```text
clud ab [OPTIONS] --backends <BACKENDS> --prompt-file <PROMPT_FILE> --runs <RUNS>
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--backends` | `<BACKENDS>` (repeatable) |  | Required. |
| `--prompt-file` | `<PROMPT_FILE>` |  | Required. |
| `--runs` | `<RUNS>` |  | Required. |
| `--model-map` | `<MODEL_MAP>` (repeatable) |  |  |
| `--json` | switch |  |  |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud remote`

Connect to an operator for consent-gated remote assistance

```text
clud remote [OPTIONS] <COMMAND>
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

Subcommands (required): `connect`.

### `clud remote connect`

Connect, share the keyword, and approve control in this terminal

```text
clud remote connect [OPTIONS] [HOST]
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `HOST` | `<HOST>` | `clud.bot` | Operator server hostname or HTTPS origin. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud clud.bot`

Connect to clud.bot for consent-gated remote assistance

```text
clud clud.bot [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud connect`

Choose full control or a model proxy, then connect to an operator

```text
clud connect [OPTIONS] [HOST]
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `HOST` | `<HOST>` | `clud.bot` | Operator server hostname or HTTPS origin. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--model` | `<MODEL>` |  | Reserved for future use; selecting a model is currently disabled. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud gate`

Run detected lint/test tools and emit one compact JSON result

```text
clud gate [OPTIONS] <LANE>
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `LANE` | `lint|test|both` |  | Required. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--repo` | `<REPO>` | `.` |  |
| `--no-fast-fail` | switch |  |  |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud serve`

Serve Claude or Codex and optional Anthropic-compatible provider models to local API clients

```text
clud serve [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--claude` | switch |  | Enable the Claude upstream. |
| `--codex` | switch |  | Enable Codex using clud's managed login or OPENAI_API_KEY. |
| `--openrouter` | switch |  | Add the configured OpenRouter upstream. |
| `--deepseek` | switch |  | Add the configured DeepSeek upstream (vault key or DEEPSEEK_API_KEY) |
| `--kimi` | switch |  | Add the configured Kimi upstream (vault key or KIMI_API_KEY) |
| `--port` | `<PORT>` | `8317` | Local port for API clients; zero chooses an available port. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud secret`

Acquire a credential in a masked window and deliver it directly to a sink

```text
clud secret [OPTIONS] --title <TITLE> --label <LABEL> <--to-file <PATH>|--vault <VAULT>|--to-command <COMMAND>...|--to-keyring <SERVICE> <USER>>
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--internal-vault-stdin` | switch |  | Internal stdin-only vault sink used by lokr. Hidden. Conflicts with `--dry-run`. |
| `--title` | `<TITLE>` |  | Name the specific credential and service/host (for example GitHub OAuth Client ID for example.com) Required. |
| `--label`, `--message` | `<LABEL>` |  | Explain why it is needed and where to find it. Required. |
| `--hint` | `<HINT>` |  | Expected format, displayed alongside the request. |
| `--dry-run` | switch |  | Preview the request text and destination without acquiring or storing a secret. |
| `--multiline` | switch |  | Accept a masked multiline value, such as a private key. |
| `--pattern` | `<REGEX>` |  | Require the entire value to match this regex; lokr limits submissions to three. |
| `--to-file` | `<PATH>` |  | Write to an owner-only file (0600 on Unix, protected DACL on Windows) |
| `--to-keyring` | `<SERVICE>` (repeatable) |  | Store in lokr’s native keyring under SERVICE and USER. |
| `--vault` | `deepseek|kimi|openrouter` |  | Store a provider credential in clud's native vault. |
| `--to-command` | `<COMMAND>` (repeatable) |  | Deliver the credential to this command's stdin; suppress its output. |

Group `sink`: exactly one of `--to-file`, `--vault`, `--to-command`, `--to-keyring`.

### `clud sudo`

Run one command as root; a needed sudo password comes from the masked prompt

```text
clud sudo [OPTIONS] <COMMAND>...
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `COMMAND...` | `<COMMAND>` (repeatable) |  | The command, optionally preceded by sudo options such as `-u USER` Required. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--reason`, `--message` | `<TEXT>` |  | Why root is needed; shown in the masked prompt. |
| `--dry-run` | switch |  | Print the prompt text and the sudo invocations without prompting or running. |

### `clud secrets-mcp` (hidden)

lokr's MCP server for `--allow-secrets` sessions

```text
clud secrets-mcp [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud secrets-daemon` (hidden)

lokr's background credential scanner

```text
clud secrets-daemon [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud computer-use`

Computer use through the pinned cua-driver: status, install, doctor, grant

```text
clud computer-use [OPTIONS] <COMMAND>
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

Subcommands (required): `status`, `install`, `doctor`, `grant`, `mcp`.

### `clud computer-use status`

Show the pinned driver, where it resolves, and what this desktop supports

```text
clud computer-use status [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--json` | switch |  | Emit stable JSON. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud computer-use install`

Provision and verify the pinned Computer Use driver (normally done on first use)

```text
clud computer-use install [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud computer-use doctor`

Run cua-driver's own diagnostics with clud's environment

```text
clud computer-use doctor [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud computer-use grant`

macOS: request Accessibility and Screen Recording for ComputerUse once

```text
clud computer-use grant [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud computer-use mcp` (hidden)

The stdio MCP server Claude Code and Codex start

```text
clud computer-use mcp [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud run`

Explicit compatibility spelling for a normal backend launch

```text
clud run [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud models`

Query clud's cached OpenRouter model and pricing catalog

```text
clud models [OPTIONS] <COMMAND>
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

Subcommands (required): `cheapest`.

### `clud models cheapest`

List the lowest-priced eligible programming models

```text
clud models cheapest [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--json` | switch |  | Emit stable machine-readable JSON. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud codex-update`

Install or update Codex through CLUD's verified standalone-installer path

```text
clud codex-update [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud dsh-update`

Install or confirm DeepSeek Harness in clud's private npm prefix

```text
clud dsh-update [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud setup`

Install clud's aliases and set up a provider in one step: interpreter aliases, then OpenRouter/Claude/Codex with its credential and saved default. Safe to rerun. Never edits shell profiles or PATH without --write-profile. Exits 0 on success, 2 for a refused request (no terminal without --yes, missing OpenRouter key), 1 on a failed step

```text
clud setup [OPTIONS] [PRODUCT]
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `PRODUCT` | `<PRODUCT>` |  | A product to install: hermes (hermes-slim) or talos. Equivalent to `clud install <product>`; a bare `clud setup` sets up clud itself. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print every step and what is already in place; change nothing. |
| `-y`, `--yes` | switch |  | Run without prompts (required without a terminal). Keeps the saved provider, or Claude, unless --provider is given. |
| `--provider` | `<PROVIDER>` |  | Provider to configure instead of asking. |
| `--write-profile` | switch |  | Also activate clud in new shells: runs the installer for this version, which owns every shell-profile and PATH edit. |
| `--import-hermes` | switch |  | Import the OpenRouter settings and key found in Hermes' config. Required for an import under --yes. OpenRouter is pay-per-token and needs no metered overrides (DD-209) |
| `--check` | switch |  | Change nothing; exit 0 when a provider is set up, 3 when `clud setup` still needs to run (no provider saved, or OpenRouter with no key). The installer uses it to offer setup right after install. Conflicts with `--dry-run`, `-y`, `--yes`, `--provider`, `--write-profile`, `--import-hermes`. |

### `clud install`

Install another agent product (`hermes` = hermes-slim, `talos`) and offer to migrate clud's provider credentials into it (#2609). `clud setup <product>` is an exact alias. See docs/architecture/install-products.md

```text
clud install [OPTIONS] <PRODUCT>
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `PRODUCT` | `<PRODUCT>` |  | Product to install: hermes (hermes-slim) or talos. Required. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print the resolved product, bootstrap and target, and change nothing. |
| `-y`, `--yes` | switch |  | Run without prompts (required without a terminal). Never migrates credentials on its own. |
| `--dir` | `<DIR>` |  | Install directory; passed to the product's own installer. |
| `--target` | `<PRODUCT_DIR>` |  | Existing install to configure; mutually exclusive with --dir. |
| `--ref` | `<REFERENCE>` |  | Repository ref the product's bootstrap is fetched from. |
| `--version` | `<VERSION>` |  | Product version, passed to the product's own installer. |
| `--no-configure` | switch |  | Install and stop without the configure step. |
| `--configure-only` | switch |  | Skip the install and only configure an existing product. |
| `--migrate-credentials` | switch |  | Migrate clud's stored provider credentials without asking. |
| `--migrate-provider` | `<ID>` (repeatable) |  | Copy only this provider (repeatable); implies credential migration. |

### `clud pr-check`

Run the repo's declared `pr_checks.commands` in order and, when all pass, record a receipt for `HEAD^{tree}` that unblocks `git push` / `gh pr create` in the PreToolUse hook (#2720)

```text
clud pr-check [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud doctor`

Report hook and alias health: stale or retired aliases in clud's own alias dirs, a foreign `clud-cmd-scan` first on PATH, and the hook matcher's tool coverage (#1954). Exits 1 when anything is wrong

```text
clud doctor [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--json` | switch |  | Print the report as one JSON document. |
| `--fix` | switch |  | Repair what doctor finds: replace stale or missing aliases in clud's own alias dirs with the running binary. Files outside those dirs are never touched. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud inject`

Inject a user message into a running daemon session: queued for the next turn boundary, or `--interrupt` to replace the running turn (docs/architecture/session-inject.md). Never prompts

```text
clud inject [OPTIONS] <SESSION> <MESSAGE>
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `SESSION` | `<SESSION>` |  | Session id (`sess-...`) returned by `session_start` / `POST /v1/sessions` Required. |
| `MESSAGE` | `<MESSAGE>` |  | The message the agent receives as its next user turn. Required. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--interrupt` | switch |  | Interrupt the running turn instead of waiting for it to end. |
| `--client-msg-id` | `<CLIENT_MSG_ID>` |  | Idempotency key; defaults to a hash of session + message. |
| `--from` | `<FROM>` |  | Who is steering (recorded on the session); defaults to the board id. |
| `--ticket` | `<TICKET>` |  | Ticket or issue reference recorded with the injection. |
| `--detach` | switch |  | Return once accepted instead of waiting for delivery. |
| `--timeout` | `<TIMEOUT>` | `600` | Seconds to wait for delivery when not detached. |
| `--json` | switch |  | Print the result as one JSON object. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud ci`

Run a CI job locally under bosn and show only the verdict and the failing steps' diagnostics (#1839)

```text
clud ci [OPTIONS] [JOB]
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `JOB` | `<JOB>` |  | Workflow job id, e.g. `test-linux-x64-unit`; omit for the PR plan. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--filter` | `<FILTER>` |  | Also print log lines matching this regex (e.g. a test module) |
| `--workflow` | `<WORKFLOW>` |  | Workflow file, when not `.github/workflows/ci.yml` |
| `--json` | switch |  | Machine-readable output. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud auth`

Manage provider credentials. Claude authentication remains owned by Claude Code and is reported as externally managed

```text
clud auth [OPTIONS] [COMMAND]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

Subcommands (optional): `login`, `status`, `logout`.

### `clud auth login`

Sign in to a provider managed by clud

```text
clud auth login [OPTIONS] <PROVIDER>
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `PROVIDER` | `codex|deepseek|kimi|openrouter|claude` |  | Required. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--acknowledge-experimental` | switch |  | Required only for Codex subscription authentication. |
| `--no-browser` | switch |  | Do not open the Codex authorization URL in a browser. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud auth status`

Show secret-free status for all providers or one provider

```text
clud auth status [OPTIONS] [PROVIDER]
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `PROVIDER` | `codex|deepseek|kimi|openrouter|claude` |  |  |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--json` | switch |  | Emit stable JSON for automation. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud auth logout`

Remove only credentials owned by clud

```text
clud auth logout [OPTIONS] <PROVIDER>
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `PROVIDER` | `codex|deepseek|kimi|openrouter|claude` |  | Required. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--json` | switch |  | Emit stable JSON for automation. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud codex-auth` (hidden)

Deprecated compatibility alias for `clud auth <action> codex`

```text
clud codex-auth [OPTIONS] <COMMAND>
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

Subcommands (required): `login`, `status`, `logout`.

### `clud codex-auth login` (hidden)

Start the experimental ChatGPT subscription sign-in flow

```text
clud codex-auth login [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--acknowledge-experimental` | switch |  | Required acknowledgement that subscription compatibility is experimental and may change independently of clud. |
| `--no-browser` | switch |  | Do not open a browser automatically; print the URL instead. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud codex-auth status` (hidden)

Show the clud-managed subscription login state without secrets

```text
clud codex-auth status [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--json` | switch |  | Emit stable JSON for automation. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud codex-auth logout` (hidden)

Remove only clud-managed subscription credentials

```text
clud codex-auth logout [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--json` | switch |  | Emit stable JSON for automation. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud deepseek-auth` (hidden)

Deprecated compatibility alias for `clud auth <action> deepseek`

```text
clud deepseek-auth [OPTIONS] <COMMAND>
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

Subcommands (required): `login`, `status`, `logout`.

### `clud deepseek-auth login` (hidden)

Prompt for and store the provider's API key in the native credential vault

```text
clud deepseek-auth login [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud deepseek-auth status` (hidden)

Report whether an API key is available without revealing it

```text
clud deepseek-auth status [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--json` | switch |  | Emit stable JSON for automation. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud deepseek-auth logout` (hidden)

Remove the clud-managed API key from the native credential vault

```text
clud deepseek-auth logout [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--json` | switch |  | Emit stable JSON for automation. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud loop`

```text
clud loop [OPTIONS] [TASK]
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `TASK` | `<TASK>` |  | Prompt text, path to a local file, or a GH issue/PR URL. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--loop-count` | `<LOOP_COUNT>` | `50` | Maximum number of iterations. |
| `--min-iterations` | `<MIN_ITERATIONS>` |  | Earliest iteration that can accept DONE. BLOCKED can still stop the loop. |
| `--refresh` | switch |  | Force re-fetch of a cached GH issue/PR body. |
| `--no-done` | switch |  | Do not inject the DONE/BLOCKED marker contract into the prompt. Conflicts with `--done`. |
| `--done` | `<DONE>` |  | Re-enable the DONE/BLOCKED contract using a custom DONE marker path. Conflicts with `--no-done`. |
| `--repeat` | `<REPEAT>` |  | Re-run the loop after it completes, sleeping for the given duration between runs (for example `30s`, `5m`, `1h`) |
| `--interval` | `<INTERVAL>` |  | Wait this long between iterations (for example `30s`, `5m`, `1h`). Without it the next iteration starts as soon as the previous one exits, which re-pays the whole prompt on every pass. |
| `--message` | `<MESSAGE>` |  | Queue a message for the next loop iteration (delivered via `.clud/loop/NEXT_MESSAGE.md`). Without a task, queue and exit. |
| `--config` | `<CONFIG>` |  | Read loop settings from this `loop.json` instead of the implicit `<git-root>/loop.json`. CLI flags override file values. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud up`

Ship it: run lint, test and cleanup, then commit with `codeup`

```text
clud up [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `-m`, `--message` | `<MESSAGE>` |  | Commit message, passed to `codeup -m` |
| `--publish` | switch |  | Also publish: run `codeup` with `-p` |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud rebase`

```text
clud rebase [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud fix`

Run the repo's lint and test tools and fix failures until they pass

```text
clud fix [OPTIONS] [URL]
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `URL` | `<URL>` |  | Optional URL (issue, PR or CI run) whose failure to fix. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud do`

Implement a URL or free-form task end-to-end under the `/goal` contract. With no target, prompt for one in an interactive foreground terminal

```text
clud do [OPTIONS] [TARGET]
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `TARGET` | `<TARGET>` |  | Issue/PR URL or free-form goal. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud grind`

Grind the current repo's issues page with `/loop`

```text
clud grind [OPTIONS] [URL]
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `URL` | `<URL>` |  | Issues page URL; default: the `origin` remote's issues page. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud video`

Edit videos with browser-use/video-use (#1851). Loads the pinned video-use skill as a plugin for this one Claude session only; ordinary `clud` sessions never see it

```text
clud video [OPTIONS] [DIR]
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `DIR` | `<DIR>` |  | Directory holding the source videos (default: current directory) |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--update` | switch |  | Reinstall the pinned video-use checkout and its Python deps, then exit. |
| `--path` | switch |  | Print the pinned video-use checkout path (installing it first if missing), then exit. The explicit-only `/video-use` skill uses it. Conflicts with `--update`. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud do-prompt` (hidden)

Install clud's bundled skills, agent types and workflows now, the same files a launch installs. `--home` targets another home directory; the real-harness tests (#1323) use it to populate an isolated config. Print the `/do` prompt for a target in this checkout: the starting- branch verdict and the contract (#1322). The `/do` skill's body runs this at invocation, so its output is what the model sees

```text
clud do-prompt [OPTIONS] [TARGET]...
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `TARGET...` | `<TARGET>` (repeatable) |  |  |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud grind-scripts` (hidden)

Print the repo's detected `./lint` / `./test` scripts for the `/grind` router (#1336). The router's body runs this at invocation

```text
clud grind-scripts [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud safe-rm`

Safely remove paths by moving them to the clud trash. Accepts rm-style flags and limits deletion to this session's allowed locations (#1461)

```text
clud safe-rm [OPTIONS] [ARGS]...
```

No `--help`: every token after the verb goes to the command.

| Argument | Value | Default | Description |
|---|---|---|---|
| `ARGS...` | `<ARGS>` (repeatable) |  |  |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud grind-facts` (hidden)

Print or clear this session's `/grind` run-facts file (#1337). The router writes its run facts there; clud's hook reads them by the payload's session id

```text
clud grind-facts [OPTIONS] [ARGS]...
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `ARGS...` | `<ARGS>` (repeatable) |  |  |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud install-assets` (hidden)

```text
clud install-assets [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--home` | `<DIR>` |  |  |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud workflow`

Run an Anthropic-format workflow script (`export const meta` + `agent()`/`parallel()`/`pipeline()`/`phase()`); every `agent()` call is a headless `clud -p` child

```text
clud workflow [OPTIONS] <SCRIPT>
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `SCRIPT` | `<SCRIPT>` |  | Path to the workflow script (`.js`) Required. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--args` | `<ARGS_JSON>` |  | JSON value exposed to the script as the global `args` |
| `--reuse` | switch |  | Reuse cached results of agent() calls whose inputs are unchanged (named `--reuse` because `--resume` belongs to the harness) |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud wasm`

Run a local `.wasm` module in clud's embedded runtime

```text
clud wasm [OPTIONS] <MODULE>
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `MODULE` | `<MODULE>` |  | Path to the `.wasm` module. Required. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--invoke` | `<INVOKE>` | `run` | Exported function to call. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud attach`

Attach to a background session, or list them when no id is given

```text
clud attach [OPTIONS] [SESSION_ID]
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `SESSION_ID` | `<SESSION_ID>` |  | Session id to attach to. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `-l`, `--last` | switch |  | Attach to the most recently created session. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud kill`

End a background session

```text
clud kill [OPTIONS] [SESSION_ID]
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `SESSION_ID` | `<SESSION_ID>` |  | Session id to end. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--all` | switch |  | End every background session. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud slay`

Kill all active background sessions

```text
clud slay [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud list`

List background, API and live sessions

```text
clud list [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--json` | switch |  | Emit API sessions as a JSON array (each row carries `origin`) |
| `--origin` | `cli|mcp|daemon|test|unknown` |  | Show only API sessions created by this entry point. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud claim`

Inspect or manage a live checkout claim

```text
clud claim [OPTIONS] <COMMAND>
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

Subcommands (required): `acquire`, `release-own`, `who`, `release`.

### `clud claim acquire`

Claim the current checkout for this live clud session

```text
clud claim acquire [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud claim release-own`

Release this session's claim without affecting another session

```text
clud claim release-own [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud claim who`

Show live sessions and their claims in the current checkout

```text
clud claim who [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud claim release`

Release a claim for a checkout after interactive confirmation

```text
clud claim release [OPTIONS] <CHECKOUT>
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `CHECKOUT` | `<CHECKOUT>` |  | Required. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--yes` | switch |  |  |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud top`

Inspect daemon-sampled CPU/RSS process trees

```text
clud top [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--json` | switch |  | Emit machine-readable JSON. |
| `--once` | switch |  | Print one snapshot and exit. |
| `--watch` | switch |  | Keep refreshing. With `--json`, prints one compact JSON object per line. Conflicts with `--once`. |
| `--tree` | switch |  | Render rows as a process tree. This is the default text mode. Conflicts with `--flat`. |
| `--flat` | switch |  | Render rows as one flat sorted table. Conflicts with `--tree`. |
| `--sort` | `cpu|mem|rss|age` | `cpu` | Sort key for tree siblings and flat rows. |
| `--limit` | `<LIMIT>` | `20` | Cap displayed rows per subtree in tree mode, or total rows in flat mode. |
| `--since` | `<DURATION>` |  | Include dead PIDs sampled within this duration, e.g. `30s`, `5m` |
| `--originator` | `<CLUD:PID>` |  | Restrict output to one cohort, e.g. `CLUD:71584` or `71584` |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud logs`

pm2-style log viewer: dump or tail a session's captured output

```text
clud logs [OPTIONS] [SESSION_ID]
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `SESSION_ID` | `<SESSION_ID>` |  | Session whose log to print; omit to list every session's last line. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `-f`, `--follow` | switch |  | Keep watching the file and print new output as it arrives. Exits once the session has terminated (after printing a status line) |
| `-n`, `--lines` | `<LINES>` |  | Print only the last N lines from the file. Default: all. |
| `-l`, `--last` | switch |  | Operate on the most recently created session (live or exited) Conflicts with `SESSION_ID`. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud gc`

Issue #110: tracked-entry garbage collection (redb-backed registry at `~/.clud/data.redb`)

```text
clud gc [OPTIONS] [COMMAND]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

Subcommands (optional): `list`, `prune`, `purge`, `all`, `reconcile`, `keep`, `branches`.

### `clud gc list`

Print tracked entries, newest first

```text
clud gc list [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--json` | switch |  | Issue #135: emit a JSON array instead of the human-readable table. |
| `--targets` | switch |  | Report daemon-cached Rust target sizes across repository worktrees. Conflicts with `--kind`. |
| `--kind` | `<KIND>` |  | Restrict to a single managed kind (e.g. `worktree`, `trash`) |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud gc prune`

Drop stale/unreferenced entries for one managed kind, or `all`

```text
clud gc prune [OPTIONS] [KIND]
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `KIND` | `<KIND>` |  | Managed kind to prune (e.g. `worktree`, `uv-cache`, `trash`), or `all` for every managed kind (issue #506) Conflicts with `--kind`. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Preview the removal plan without touching anything. |
| `--older-than` | `<DURATION>` |  | Only affect entries created longer ago than this duration (e.g. `2d`, `48h`). Overrides the per-kind default prune window. |
| `--kind` | `<KIND>` |  | Compatibility alias for the positional `KIND` |

### `clud gc purge`

Remove all entries for one managed kind, or `all`. Destructive; requires `--yes`

```text
clud gc purge [OPTIONS] [KIND]
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `KIND` | `<KIND>` |  | Managed kind to purge (e.g. `worktree`, `uv-cache`, `trash`), or `all` for every managed kind (issue #506) Conflicts with `--kind`. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Preview the removal plan without touching anything. |
| `-y`, `--yes` | switch |  | Skip the interactive confirmation prompt. |
| `--older-than` | `<DURATION>` |  | Only affect entries created longer ago than this duration (e.g. `2d`, `48h`). Without it, purge removes every entry. |
| `--kind` | `<KIND>` |  | Compatibility alias for the positional `KIND` |

### `clud gc all`

Operate across every managed kind. Defaults to safe prune

```text
clud gc all [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--purge` | switch |  | Purge every managed kind instead of pruning stale entries. |
| `--dry-run` | switch |  | Preview the removal plan without touching anything. |
| `-y`, `--yes` | switch |  | Required with `--purge` |
| `--older-than` | `<DURATION>` |  | Only affect entries created longer ago than this duration (e.g. `2d`, `48h`) |

### `clud gc reconcile`

Walk `.claude/worktrees/` in the current repo and insert any previously-untracked worktree directories

```text
clud gc reconcile [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud gc keep`

Issue #2101: pin a worktree (default: the current directory) so the daemon never reclaims it. Bare `--list` prints the kept paths

```text
clud gc keep [OPTIONS] [PATH]
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `PATH` | `<PATH>` |  | Path to keep. Defaults to the current directory. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--remove` | switch |  | Release a previously kept path instead. |
| `--list` | switch |  | Print the kept paths. Conflicts with `PATH`, `--remove`. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud gc branches`

DD-230: judge this checkout's local branches with squash-aware evidence (merged PR head or patch match) and, with `--retire`, switch back to the default branch and delete the landed ones

```text
clud gc branches [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--retire` | switch |  | Switch off a landed current branch, fast-forward the default branch, and delete every landed local branch (and its `origin` copy) |
| `--keep-remote` | switch |  | With `--retire`, leave the `origin` copies alone. |
| `--sweep-remote` | switch |  | Also judge `origin` branches that have no local branch; with `--retire`, delete the ones a merged PR's exact head proves landed. |
| `--no-fetch` | switch |  | Do not `git fetch --prune origin` first. |
| `--json` | switch |  | Emit JSON instead of the table. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud config`

Inspect or edit clud settings

```text
clud config [OPTIONS] [COMMAND]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

Subcommands (optional): `set`, `show`, `edit`.

### `clud config set`

Set a dotted settings key using the existing native approval flow

```text
clud config set [OPTIONS] <ASSIGNMENT>
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `ASSIGNMENT` | `<ASSIGNMENT>` |  | Assignment, for example context.hermes.enabled=true. Required. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud config show`

Print global, local, and merged settings

```text
clud config show [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--json` | switch |  | Emit machine-readable JSON. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud config edit`

Open a settings file in an editor

```text
clud config edit [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--local` | switch |  | Edit repo-local .clud/settings.json instead of ~/.clud/settings.json. |
| `--editor` | `<CMD>` |  | Editor command to run, e.g. `code --wait` |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud extern`

Manage the trust allowlist for foreign checkouts' hooks (zackees/clud#967 Phase 4, #966 D9)

```text
clud extern [OPTIONS] [COMMAND]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

Subcommands (optional): `trust`.

### `clud extern trust`

Trust an extern checkout so its own hooks fire for its files

```text
clud extern trust [OPTIONS] [NAME]
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `NAME` | `<NAME>` |  | The checkout's directory name under the repo's extern directory. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--list` | switch |  | Print the recorded allow entries (optionally only for `name`) and exit without changing anything. |
| `--revoke` | switch |  | Remove the allow entry for `name` instead of adding one. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud ui`

Issue #183: open the local web dashboard served by the always-on clud daemon. Shows live sessions, garbage tracking, and the repos clud has been launched in. Loopback only

```text
clud ui [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--json` | switch |  | Print `/state.json` to stdout and exit without launching a browser. |
| `--no-open` | switch |  | Print the dashboard URL and ensure the daemon is up, but do not launch a browser. Handy when running on a headless host. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud trash`

Quarantine paths under ~/.clud/trash and let daemon GC reap them

```text
clud trash [OPTIONS] <PATH>...
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `PATH...` | `<PATH>` (repeatable) |  | Required. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--cross-volume` | switch |  | Allow copy + best-effort source removal when source and trash live on different volumes. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud tool`

Run and inspect bundled clud tools without starting the daemon

```text
clud tool [OPTIONS] <COMMAND>
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

Subcommands (required): `run`, `list`, `log`, `ledger`, `info`.

### `clud tool run`

Invoke a bundled tool by its `~/.clud/tools/`-relative path, forwarding any trailing args to the tool. Example: `clud tool run github/pr_merge_watch.py 404`. A resumable watchdog timeout returns 124 with `status: in-progress` on stderr; invoke the same tool again to keep watching. Exit 0 means the tool itself completed successfully

```text
clud tool run [OPTIONS] <REL_PATH> [ARGS]...
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `REL_PATH` | `<REL_PATH>` |  | Path under `~/.clud/tools/` (e.g. `github/pr_merge_watch.py`) Required. |
| `ARGS...` | `<ARGS>` (repeatable) |  | Arguments forwarded verbatim to the tool. Use `--` to pass flags the clud parser would otherwise consume. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud tool list`

List tool invocations in the current clud session — slice 3 of #427

```text
clud tool list [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--json` | switch |  | Emit a JSON array instead of the human-readable table. |
| `--long` | switch |  | Show the long-form `<session-pid>-<tool-id>` ID in the table. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud tool log`

Query the full JSONL log of one invocation with optional filters — slice 4 of #427

```text
clud tool log [OPTIONS] [REFERENCE]
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `REFERENCE` | `<REFERENCE>` |  | Reference to the invocation. Same forms as `tool info` |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--pid` | `<PID>` |  | Look up by the tool's own OS PID instead of session-local id. |
| `--stream` | `<STREAM>` | `combined` | Which stream to read: `stdout`, `stderr`, or `combined` (default) |
| `--since` | `<SINCE>` |  | Only entries newer than `now - <duration>` (e.g. `5m`, `1h`) |
| `--until` | `<UNTIL>` |  | Only entries older than `now - <duration>` |
| `--between` | `<BETWEEN>` (repeatable) |  | Absolute time range as two integer epoch-ms values. |
| `--grep` | `<GREP>` |  | Substring match on the decoded line text. |
| `--head` | `<HEAD>` |  | Show only the first N matching entries. |
| `--tail` | `<TAIL>` |  | Show only the last N matching entries. |
| `--json` | switch |  | Emit the raw JSONL stream instead of decoded text. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud tool ledger`

History of tool invocations matching optional filters — slice 4 of #427

```text
clud tool ledger [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--tool` | `<TOOL>` |  | Restrict to one tool name. |
| `--session` | `<SESSION>` | `current` | Session scope: `current` (default), `previous`, or `all` |
| `--json` | switch |  | Emit a JSON array instead of the human-readable table. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud tool info`

Show current state + last N lines of stdout/stderr for one invocation — slice 3 of #427

```text
clud tool info [OPTIONS] [REFERENCE]
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `REFERENCE` | `<REFERENCE>` |  | Reference to the invocation. Accepts: * a bare session-local integer (`3`) * a long-form `<session-pid>-<tool-id>` (`47180-3`) * `@<tool-name>` or `@<tool-name>:N` for N-th-most-recent Omit to default to the most recently started invocation. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--pid` | `<PID>` |  | Look up by the tool's own OS PID instead of the session-local integer. Bare integers always mean tool-id, not PID — use this flag to disambiguate. |
| `--lines` | `<LINES>` | `20` | Number of trailing stdout/stderr lines to show per stream. |
| `--json` | switch |  | Emit a JSON object instead of the human-readable view. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud log`

Issue #469 (beta prototype): POST one telemetry event to the always-on clud daemon's HTTP server. Captures parent PID, time, the `cmd` string passed in, the current working directory, and every env var beginning with `CLUD_`. The daemon URL is read from `$CLUD_DAEMON_HTTP_SERVER`. By default missing env / unreachable daemon are silent (exit 0) so a hook caller is never broken; with `--fail-on-no-server` either failure causes a non-zero exit so tests can prove a real round-trip

```text
clud log [OPTIONS] --cmd <CMD>
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `-c`, `--cmd` | `<CMD>` |  | Free-form command string describing what the caller was doing. Stored verbatim in the telemetry record. Required. |
| `--fail-on-no-server` | switch |  | Exit non-zero if `CLUD_DAEMON_HTTP_SERVER` is unset OR the POST fails. Without this flag, failures are swallowed. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud optimize`

Install and persist fast local tooling defaults

```text
clud optimize [OPTIONS] [TARGET]
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `TARGET` | `rust` | `rust` | Toolchain family to optimize. Defaults to Rust. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--global` | switch |  | Persist the recommendation in ~/.clud/settings.json. Conflicts with `--repo`. |
| `--repo` | switch |  | Write a repo-local .clud/settings.json directive. Conflicts with `--global`. |
| `--install-soldr` | `[=true|false]` | `true` | Install soldr if it is missing from PATH. |
| `--use-soldr-shims` | `[=true|false]` | `true` | Enable soldr shims for future clud-managed Rust setup. |
| `--soldr-version` | `<SOLDR_VERSION>` | `latest` | Ignored with a warning: soldr cannot be pinned and always follows latest. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud daemon`

Control the always-on clud daemon

```text
clud daemon [OPTIONS] <COMMAND>
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

Subcommands (required): `api-info`, `restart`, `stop`, `running-process`, `orphan-status`.

### `clud daemon api-info`

Print the private loopback API discovery document

```text
clud daemon api-info [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--json` | switch |  |  |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud daemon restart`

Restart the daemon process so the next CLI call uses the current binary

```text
clud daemon restart [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud daemon stop`

Stop the daemon if it is running without starting a replacement

```text
clud daemon stop [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud daemon running-process`

Print the current running-process adoption preview

```text
clud daemon running-process [OPTIONS]
```

Aliases: `servicedef`

| Flag | Value | Default | Description |
|---|---|---|---|
| `--json` | switch |  | Emit machine-readable JSON. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud daemon orphan-status`

Report the last orphan-sweep result and freshness (#465). Exits non-zero if no sweep has run within 2× the sweep interval

```text
clud daemon orphan-status [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--json` | switch |  | Emit machine-readable JSON. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud symbols`

Inspect or verify crash-report symbolication (#374 PR 3/3)

```text
clud symbols [OPTIONS] [COMMAND]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

Subcommands (optional): `install`, `verify`.

### `clud symbols install`

Verify that the running binary's embedded line tables resolve recent crash report backtraces. With the embed-everywhere strategy, this is a no-op when symbols are already present and a diagnostic when they're not. Exits 1 if any inspected report is unsymbolicated

```text
clud symbols install [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud symbols verify`

Same as `install` but with an explicit `--all` toggle

```text
clud symbols verify [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--all` | switch |  | Verify every report under `~/.clud/state/crashes/` rather than just the most-recent one. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud test`

Record and report per-bucket test runtimes for this checkout (#407)

```text
clud test [OPTIONS] <COMMAND>
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

Subcommands (required): `run`, `capture`, `stats`.

### `clud test run`

Run a test command, recording its duration and the pre-run CPU load

```text
clud test run [OPTIONS] [COMMAND]...
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `COMMAND...` | `<COMMAND>` (repeatable) |  | The command to run, after `--` |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--bucket` | `<BUCKET>` | `unit` | Bucket this run belongs to: unit \| integration \| e2e \| smoke. |
| `--target` | `<TARGET>` |  | Optional test filter/target, recorded alongside the duration so a future query can localize "which one is slow". |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud test capture` (hidden)

Capture test output for the bounded report tool

```text
clud test capture [OPTIONS] --output-dir <OUTPUT_DIR> [COMMAND]...
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `COMMAND...` | `<COMMAND>` (repeatable) |  | Command to execute, after `--` |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--output-dir` | `<OUTPUT_DIR>` |  | Directory verified by the host-specific privacy implementation. Required. |
| `--timeout-secs` | `<TIMEOUT_SECS>` |  | Explicit time limit for the child and its output writers (default: none) |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud test stats`

Report per-bucket runtime statistics

```text
clud test stats [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--bucket` | `<BUCKET>` |  | Limit the report to one bucket. |
| `--json` | switch |  | Machine-readable output for agent consumption. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud settings`

Interactively toggle global clud settings (zackees/clud)

```text
clud settings [OPTIONS]
       clud settings <COMMAND>
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--list` | switch |  | Print current settings and exit; no TUI, no raw terminal mode. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

Flags and subcommands are mutually exclusive here.

Subcommands (optional): `set`, `roots`.

### `clud settings set`

Set one JSON pointer after native user approval; preserves deletion roots

```text
clud settings set [OPTIONS] --reason <REASON> <POINTER> <VALUE>
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `POINTER` | `<POINTER>` |  | JSON pointer, for example /web_term/enabled. Required. |
| `VALUE` | `<VALUE>` |  | JSON value, for example false or a quoted string. Required. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--reason` | `<REASON>` |  | Explain this specific requested change to the user. Required. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud settings roots`

Replace deletion roots after separate, explicit native user approval

```text
clud settings roots [OPTIONS] --reason <REASON> <VALUE>
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `VALUE` | `<VALUE>` |  | JSON array of objects containing an absolute path and non-empty reason. Required. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--reason` | `<REASON>` |  | Explain this specific requested change to the user. Required. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud statusline` (hidden)

Internal (#1189): print clud's toast inside Claude Code's status line. Claude Code runs this as the injected `statusLine` command

```text
clud statusline [OPTIONS] --session-pid <SESSION_PID> --state-dir <STATE_DIR>
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--session-pid` | `<SESSION_PID>` |  | Required. |
| `--state-dir` | `<STATE_DIR>` |  | Required. |
| `--chain-b64` | `<CHAIN_B64>` |  | The user's own status-line command, base64url, run first. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud session`

Inspect the current agent session (#2122)

```text
clud session [OPTIONS] <COMMAND>
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

Subcommands (required): `stats`, `new`, `show`.

### `clud session stats`

The session's edit/lint/build/test/CI breakdown with Bash wall time, as clud's verify-ratio hook recorded it

```text
clud session stats [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--session` | `<SESSION>` |  | Claude Code session id; defaults to `CLAUDE_CODE_SESSION_ID` |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud session new`

Create a durable API session from the CLI (recorded as `origin: cli`) and print its id

```text
clud session new [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--backend` | `<BACKEND>` | `claude` | Harness backend: `claude` or `codex` |
| `--cwd` | `<CWD>` |  | Working directory; defaults to the current directory. |
| `--name` | `<NAME>` |  | Display name. |
| `--model` | `<MODEL>` |  | Model to pin. |
| `--safe` | switch |  | Run the session without the permission bypass. |
| `--json` | switch |  | Print the created record as JSON. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud session show`

Print one API session's state, origin, backend and model

```text
clud session show [OPTIONS] <ID>
```

| Argument | Value | Default | Description |
|---|---|---|---|
| `ID` | `<ID>` |  | API session id (`sess-...`) Required. |

| Flag | Value | Default | Description |
|---|---|---|---|
| `--json` | switch |  | Emit the stored record as JSON. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud session-hook` (hidden)

Internal (#922): Claude lifecycle hook that keeps clud's per-cwd session index current. Claude Code runs this; it reads the hook payload on stdin and never fails the session

```text
clud session-hook [OPTIONS] --event <EVENT> --route <ROUTE> --state-dir <STATE_DIR>
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--event` | `<EVENT>` |  | Required. |
| `--route` | `<ROUTE>` |  | The launch's route: a provider name, or `unified` Required. |
| `--state-dir` | `<STATE_DIR>` |  | Required. |
| `--recovery-file` | `<RECOVERY_FILE>` |  | Portable recovery context to inject at `SessionStart` |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud __start-cmd` (hidden)

Open the explicitly requested persistent Command Prompt

```text
clud __start-cmd [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud __openai-chat-translate` (hidden)

Test seam for the OpenAI Chat Completions ingress: read one OpenAI-shaped request body on stdin, write the translated Messages request body to stdout. With `--response` it runs the other direction instead (Anthropic Messages response in, OpenAI completion out). Never used by a launched session

```text
clud __openai-chat-translate [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--response` | switch |  | Translate an Anthropic Messages response, not an OpenAI request. |
| `--stream` | switch |  | Read the response as SSE and emit one chat.completion.chunk per line. |
| `--for-model` | `<FOR_MODEL>` |  | Model to report when the response body carries none. |
| `--http` | switch |  | Emit the exact HTTP body the route writes, not one JSON per line: a chat.completion body, or `data: <chunk>` frames plus `[DONE]` |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud __gemini-client-translate` (hidden)

```text
clud __gemini-client-translate [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--model` | `<MODEL>` |  | Model to serve the request with; without it the request's own model wins. |
| `--response` | switch |  | Translate an Anthropic Messages response into a Gemini response. |
| `--stream` | switch |  | Read the response as SSE and emit one Gemini response chunk per line. |
| `--http` | switch |  | Emit the exact HTTP body a route writes, not one JSON per line. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud __claude-gemini-translate` (hidden)

Test seam for the Claude <-> Gemini translator: read one Anthropic Messages request body on stdin, write the translated Gemini `generateContent` request body to stdout. `--model` names the target Gemini model; without it the request's own model is kept. With `--response` it runs the other direction instead (a Gemini `generateContent` response in, the Anthropic Messages response out). Never used by a launched session

```text
clud __claude-gemini-translate [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--model` | `<MODEL>` |  | Target Gemini model id. |
| `--response` | switch |  | Translate a Gemini response, not an Anthropic request. |
| `--stream` | switch |  | Read the response as SSE and emit one Anthropic event per line. |
| `--http` | switch |  | Emit the exact HTTP body a route writes (a Messages body, or `event: <kind>` frames), not one JSON object per line. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud __apply-payload-rules` (hidden)

Test seam for the JSON-path payload rules: read `{"rules": ..., "payload": ...}` on stdin (or the rules from `--rules` and the payload on stdin), write the rewritten payload and what each rule kind did to stdout. Never used by a launched session

```text
clud __apply-payload-rules [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--rules` | `<RULES>` |  | Read the rules from this file instead of the stdin document. This is the same loader `CLUD_PAYLOAD_RULES` uses at gateway time. |
| `--model` | `<MODEL>` |  | Model the rules select on. Absent matches no `models` entry, so only model-agnostic rules fire. |
| `--protocol` | `<PROTOCOL>` | `claude` | Protocol the rules select on (`claude` for `POST /v1/messages`) |
| `--from-protocol` | `<FROM_PROTOCOL>` | `` | Protocol the *client* spoke (`openai` for `POST /v1/chat/completions`), which a rule's `from-protocol` selector matches. Empty means unknown. |
| `--header` | `<NAME: VALUE>` (repeatable) |  | A request header the rules can select on, as `Name: value`. Repeatable; mirrors the headers a real request carries. |
| `--settings-file` | `<SETTINGS_FILE>` |  | Read the rules from `requests.payload` in this settings file instead of the stdin document — the owner the gateway uses when `CLUD_PAYLOAD_RULES` is unset. Conflicts with `--rules`. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud __openai-completions-translate` (hidden)

Hidden test seam for the legacy completions ingress. Without `--response` it translates a `/v1/completions` request into a Messages request; with it, the reverse: an upstream Messages body into a `text_completion` body or chunk stream

```text
clud __openai-completions-translate [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--response` | switch |  | Translate an Anthropic Messages response, not an OpenAI request. |
| `--stream` | switch |  | Read the response as SSE and emit one chunk per line. |
| `--for-model` | `<FOR_MODEL>` |  | Model to report when the response body carries none. |
| `--http` | switch |  | Emit the exact HTTP body the route writes, not one JSON per line. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud __openai-responses-translate` (hidden)

Hidden test seam for the Responses ingress. Without `--response` it translates a Responses request into a Messages request; with it, the reverse: an upstream Messages body into a Responses `response` object or event stream

```text
clud __openai-responses-translate [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--response` | switch |  | Translate an Anthropic Messages response, not an OpenAI request. |
| `--stream` | switch |  | Read the response as SSE and emit one `response.*` event per line. |
| `--for-model` | `<FOR_MODEL>` |  | Model to report when the response body carries none. |
| `--http` | switch |  | Emit the exact HTTP body the route writes, not one JSON per line. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud __xai-device-login` (hidden)

Hidden test seam for xAI's RFC 8628 device login: run discovery, request a device code, poll until authorized, and print progress as JSON lines. Never used by a launched session

```text
clud __xai-device-login [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--discovery-url` | `<DISCOVERY_URL>` |  | Discovery document URL (defaults to xAI's) |
| `--device-endpoint` | `<DEVICE_ENDPOINT>` |  | Device authorization endpoint, skipping discovery. |
| `--token-endpoint` | `<TOKEN_ENDPOINT>` |  | Token endpoint, skipping discovery. |
| `--interval-ms` | `<INTERVAL_MS>` |  | Minimum poll interval in milliseconds. |
| `--max-poll-ms` | `<MAX_POLL_MS>` |  | Bound on the whole polling wait, in milliseconds. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud __kimi-device-login` (hidden)

Hidden test seam for Kimi Code's RFC 8628 device login: request a device code, poll until authorized, and print progress as JSON lines. With `--refresh-stored` it renews the login already in the OS vault instead. Never used by a launched session

```text
clud __kimi-device-login [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--domain` | `<DOMAIN>` |  | Kimi domain: `com` (default) or `ai` |
| `--device-endpoint` | `<DEVICE_ENDPOINT>` |  | Device authorization endpoint, overriding the compiled-in one. |
| `--token-endpoint` | `<TOKEN_ENDPOINT>` |  | Token endpoint, overriding the compiled-in one. |
| `--interval-ms` | `<INTERVAL_MS>` |  | Minimum poll interval in milliseconds. |
| `--max-poll-ms` | `<MAX_POLL_MS>` |  | Bound on the whole polling wait, in milliseconds. |
| `--refresh-stored` | switch |  | Refresh the login stored in the OS vault instead of starting a new one. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud __meta-device-login` (hidden)

Hidden test seam for Meta Muse's RFC 8628 device login: request a device code, poll until the account authorizes it, mint the Meta Model API key, store the credential in the OS vault and print progress as JSON lines. `--status` prints the stored (non-secret) record without a network call. Never used by a launched session

```text
clud __meta-device-login [OPTIONS]
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--device-endpoint` | `<DEVICE_ENDPOINT>` |  | Device authorization endpoint, overriding the compiled-in one. |
| `--token-endpoint` | `<TOKEN_ENDPOINT>` |  | Token endpoint, overriding the compiled-in one. |
| `--mint-url` | `<MINT_URL>` |  | API-key mint endpoint, overriding the compiled-in one. |
| `--interval-ms` | `<INTERVAL_MS>` |  | Minimum poll interval in milliseconds. |
| `--max-poll-ms` | `<MAX_POLL_MS>` |  | Bound on the whole polling wait, in milliseconds. |
| `--status` | switch |  | Print the stored credential summary instead of starting a login. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud __daemon` (hidden)

```text
clud __daemon [OPTIONS] --state-dir <STATE_DIR>
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--state-dir` | `<STATE_DIR>` |  | Required. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |

### `clud __worker` (hidden)

```text
clud __worker [OPTIONS] --state-dir <STATE_DIR> --session-id <SESSION_ID> --daemon-pid <DAEMON_PID> --spec-file <SPEC_FILE>
```

| Flag | Value | Default | Description |
|---|---|---|---|
| `--state-dir` | `<STATE_DIR>` |  | Required. |
| `--session-id` | `<SESSION_ID>` |  | Required. |
| `--daemon-pid` | `<DAEMON_PID>` |  | Required. |
| `--spec-file` | `<SPEC_FILE>` |  | Required. |
| `--dry-run` | switch |  | Print what would run (the launch plan as JSON) and exit. Global. |


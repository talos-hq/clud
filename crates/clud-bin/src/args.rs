use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

use crate::backend::{HarnessSelection, ModelProvider, RoutingMode};
use crate::graphics::GraphicsMode;

/// Fast CLI for running supported agent harnesses in YOLO mode.
#[derive(Parser, Clone)]
#[command(
    name = "clud",
    version,
    about = "Fast CLI for running supported agent harnesses in YOLO mode",
    after_help = "Unrelated backend flags are forwarded; use -- before backend arguments to bypass clud flag validation."
)]
pub struct Args {
    /// Open this backend launch in clud's owned web terminal window.
    #[arg(long = "web-term", conflicts_with = "set_web_term")]
    pub web_term: bool,

    /// Open this backend launch in the Kitty-compatible desktop terminal.
    #[arg(long = "kitty-term", conflicts_with_all = ["web_term", "set_web_term"])]
    pub kitty_term: bool,

    /// Persist the web-terminal preference. With no value this enables it;
    /// pass `off` to restore the ordinary console launch.
    #[arg(
        long = "set-web-term",
        value_enum,
        num_args = 0..=1,
        default_missing_value = "on",
        conflicts_with = "web_term"
    )]
    pub set_web_term: Option<WebTermPreference>,

    #[arg(short = 'p', long = "prompt")]
    pub prompt: Option<String>,

    #[arg(short = 'm', long = "message")]
    pub message: Option<String>,

    #[arg(short = 'c', long = "continue")]
    pub continue_session: bool,

    /// Resume a session, or open the picker when no value is supplied. Use
    /// `--resume=<session>` when the session name matches a built-in verb.
    #[arg(short = 'r', long = "resume")]
    pub resume: Option<Option<String>>,

    /// Continue the newest Claude-harness session in this directory without
    /// opening the `-c` picker (#922).
    #[arg(long = "last")]
    pub last: bool,

    /// How `-c`/`--last` resumes the selected session: `auto` (native when
    /// safe, portable recovery otherwise), `native` (Claude's own resume), or
    /// `portable` (a new session rebuilt from the transcript).
    #[arg(long = "resume-mode", value_enum, default_value_t)]
    pub resume_mode: crate::session_history::recover::ResumeMode,

    #[arg(long = "claude", conflicts_with_all = ["codex", "deepseek", "kimi", "openrouter"])]
    pub claude: bool,

    #[arg(long = "codex", conflicts_with_all = ["claude", "deepseek", "kimi", "openrouter"])]
    pub codex: bool,

    /// Use DeepSeek's Anthropic-compatible API through the Claude harness.
    #[arg(long = "deepseek", conflicts_with_all = ["claude", "codex", "kimi", "openrouter"])]
    pub deepseek: bool,

    /// Use Kimi's Anthropic-compatible API through the Claude harness.
    #[arg(long = "kimi", conflicts_with_all = ["claude", "codex", "deepseek", "openrouter", "provider", "unified", "mode"])]
    pub kimi: bool,

    /// Use OpenRouter's Anthropic-compatible API through the Claude harness.
    #[arg(long = "openrouter", conflicts_with_all = ["claude", "codex", "deepseek", "kimi", "provider", "unified", "mode"])]
    pub openrouter: bool,

    /// Select a provider using the generic script-friendly spelling.
    #[arg(long = "provider", value_enum, conflicts_with_all = ["claude", "codex", "deepseek", "kimi", "openrouter"])]
    pub provider: Option<ModelProvider>,

    /// Route configured providers through one Claude model picker.
    #[arg(long = "unified", conflicts_with_all = ["claude", "codex", "deepseek", "kimi", "openrouter", "provider", "mode"])]
    pub unified: bool,

    /// Generic spelling for the launch routing mode.
    #[arg(long = "mode", value_parser = ["unified"], conflicts_with_all = ["claude", "codex", "deepseek", "kimi", "openrouter", "provider", "unified"])]
    pub mode: Option<String>,

    /// Ordered fallback routes for `--unified`, tried when the active route is
    /// exhausted, drained, or rejects its credential. Comma-separated, in
    /// descent order, e.g. `--failover claude-opus-4-1,codex-terra`.
    #[arg(long = "failover", value_name = "ROUTES")]
    pub failover: Option<String>,

    /// Allow descending onto a rung billed per token. Without it, metered rungs
    /// are listed but never taken, so automatic recovery cannot become an
    /// automatic charge.
    #[arg(long = "failover-allow-metered")]
    pub failover_allow_metered: bool,

    /// Select the agent harness independently from the model provider.
    #[arg(long = "harness", value_enum)]
    pub harness: Option<HarnessSelection>,

    #[arg(long = "subprocess", conflicts_with = "pty")]
    pub subprocess: bool,

    #[arg(long = "pty", conflicts_with = "subprocess")]
    pub pty: bool,

    /// Control terminal graphics headers for PTY sessions.
    #[arg(long = "graphics", value_enum, default_value_t = GraphicsMode::Auto)]
    pub graphics: GraphicsMode,

    /// Render this image as the PTY graphics header when Sixel is enabled.
    #[arg(long = "graphics-image", value_name = "PATH")]
    pub graphics_image: Option<PathBuf>,

    /// Render the bundled README hero image as a standalone Sixel demo and exit.
    #[arg(long = "demo-gfx-sixel")]
    pub demo_gfx_sixel: bool,

    #[arg(long = "model")]
    pub model: Option<String>,

    /// Constrain every model this launch can reach -- the main model, the
    /// haiku/background and subagent slots, the rows gateway discovery may
    /// advertise, and anything a bridge will serve -- to an explicit set.
    /// Repeatable. Without it, `--model <id>` alone is the allowlist; with
    /// neither, nothing is constrained and behavior is unchanged (#1257).
    #[arg(long = "allow-model", value_name = "MODEL", action = ArgAction::Append)]
    pub allow_model: Vec<String>,

    /// OpenRouter only: serve every request from exactly these upstream
    /// providers (OpenRouter's `provider.only`, e.g. `parasail/fp8`), with
    /// fallbacks off, so a request fails rather than rerouting to another
    /// provider or price. Repeatable.
    #[arg(long = "provider-only", value_name = "PROVIDER", action = ArgAction::Append)]
    pub provider_only: Vec<String>,

    /// OpenRouter only: restrict routing to zero-data-retention endpoints.
    #[arg(long)]
    pub zdr: bool,

    /// Save OpenRouter zero-data-retention routing for future launches and exit.
    /// With no value enables it; pass off to disable the saved requirement.
    #[arg(
        long = "set-openrouter-zdr",
        value_enum,
        num_args = 0..=1,
        default_missing_value = "on",
        conflicts_with_all = ["dry_run", "zdr"]
    )]
    pub set_openrouter_zdr: Option<OpenRouterZdrPreference>,

    /// Resolved user preference; never accepted from backend arguments.
    #[arg(skip)]
    pub saved_openrouter_zdr: Option<bool>,

    /// OpenRouter only: exclude endpoints that collect request data.
    #[arg(long)]
    pub no_data_collection: bool,

    /// Tokenize credentials before provider requests and restore them in responses.
    #[arg(long, conflicts_with = "no_redact_secrets")]
    pub redact_secrets: bool,

    /// Disable automatic reversible secret redaction, including free models.
    #[arg(long, conflicts_with = "redact_secrets")]
    pub no_redact_secrets: bool,

    /// Reasoning effort, kept independent from the selected model.
    #[arg(long = "effort")]
    pub effort: Option<String>,

    /// Requested context window (for example `1m`) where the selected model supports it.
    #[arg(long = "context-window")]
    pub context_window: Option<String>,

    /// Disable the launch-level preference to finish work through a PR.
    #[arg(long = "no-land")]
    pub no_land: bool,

    #[arg(long = "safe", conflicts_with = "unsafe_mode")]
    pub safe: bool,

    /// Disable clud's launch-scoped agent safety policy, including deletion
    /// redirects and command guards. Explicit safe-rm calls retain their meaning.
    #[arg(long = "unsafe", conflicts_with = "safe")]
    pub unsafe_mode: bool,

    /// Strip the harness tools that stall an unattended run waiting on a
    /// human: plan mode and multiple-choice questions. `--dangerously-skip-
    /// permissions` does not cover these — the model volunteers them on its
    /// own, most visibly at the top of each `/loop` iteration, and the run
    /// then sits idle until someone approves the plan.
    ///
    /// Claude harness only; Codex has no equivalent tool surface.
    #[arg(long = "unattended")]
    pub unattended: bool,

    /// Allow automatic plan mode during native /loop or scheduled work.
    /// Also restore plan mode on the Codex-provider /
    /// Claude-harness bridge, where clud otherwise disallows `EnterPlanMode`
    /// unconditionally.
    ///
    /// The bridge suppresses it because the harness hands the model an
    /// `EnterPlanMode` tool whose own description tells it to reach for plan
    /// mode proactively on any non-trivial implementation ask — so a plain
    /// question turns into an unrequested planning session. Suppression is not
    /// tied to `--unattended` here: it applies to interactive bridge runs too.
    ///
    /// `AskUserQuestion` is deliberately left alone; only plan mode is stripped.
    #[arg(long = "allow-plan-mode")]
    pub allow_plan_mode: bool,

    /// Opt in to Claude Code commit/PR attribution (#1317). clud hides the
    /// `Co-Authored-By: Claude …` trailer and "Generated with Claude Code"
    /// PR line by default; `--coauthor` keeps Claude Code's own, and
    /// `--coauthor=TAG` uses TAG for both. `CLUD_COAUTHOR=1|TAG` does the
    /// same. Claude harness only; other harnesses ignore it.
    #[arg(
        long = "coauthor",
        value_name = "TAG",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = ""
    )]
    pub coauthor: Option<String>,

    #[arg(long = "dry-run", global = true)]
    pub dry_run: bool,

    /// Set by `main` for `clud do`: the target is a meta issue (it has open
    /// native sub-issues), so the launch requires grind's harness capabilities.
    /// Never a CLI flag; see `command::do_kind`.
    #[arg(skip)]
    pub do_meta: bool,

    /// Live `/do` procedure prepared before launch setup. Never a CLI flag:
    /// the builder wraps this text in `/goal`, without nested skill expansion.
    #[arg(skip)]
    pub do_rendered_prompt: Option<String>,

    #[arg(long = "detach", conflicts_with = "dry_run")]
    pub detach: bool,

    #[arg(long = "detachable", conflicts_with = "dry_run")]
    pub detachable: bool,

    #[arg(long = "name")]
    pub session_name: Option<String>,

    /// Write daemon-managed session output bytes to a transcript file.
    /// Implies centralized daemon execution.
    #[arg(long = "transcript", value_name = "PATH")]
    pub transcript: Option<PathBuf>,

    /// Override the in-memory attach-replay backlog cap. Accepts bytes
    /// (`262144`), or SI/binary suffixes (`256k`, `256KiB`, `1mb`). The
    /// compiled default is 256 KiB. Also honored as `CLUD_BACKLOG_BYTES`.
    #[arg(long = "backlog-size")]
    pub backlog_size: Option<String>,

    #[arg(short = 'v', long = "verbose")]
    pub verbose: bool,

    /// Disable the Windows console drag-and-drop target registration.
    /// Issue #79: by default `clud` registers an `IDropTarget` on the
    /// console window so dragged files are forwarded to the backend.
    /// Pass `--no-dnd` to opt out (no-op on POSIX, where drops already
    /// arrive as bracketed-paste stdin bytes).
    #[arg(long = "no-dnd", alias = "no-drag-drop")]
    pub no_dnd: bool,

    /// Issue #83: enumerate this repo's git worktrees and remove the
    /// stale ones. Combine with `--dry-run` to preview, `--yes` to skip
    /// confirmation, `--force` to also remove dirty/unpushed worktrees.
    #[arg(long = "clean-worktrees")]
    pub clean_worktrees: bool,

    /// Inspect Claude/Codex PreToolUse hook parity and apply explicit,
    /// repo-scoped repairs where clud can do so safely.
    #[arg(long = "fix-hooks")]
    pub fix_hooks: bool,

    /// Globally disable automatic deterministic hook-health repairs on launch.
    #[arg(long = "no-fix-hooks", conflicts_with = "fix_hooks")]
    pub no_fix_hooks: bool,

    /// Issue #83: minimum age before a clean worktree is treated as stale.
    /// Accepts `30s`, `5m`, `2h`, `1d`. Defaults to `1d`.
    #[arg(long = "stale-after", default_value = "1d")]
    pub stale_after: String,

    /// Issue #83: skip interactive confirmation prompts (combined with
    /// `--clean-worktrees`).
    #[arg(long = "yes", short = 'y')]
    pub yes: bool,

    /// Open the native clud installer selector before ordinary startup.
    #[arg(long = "installer")]
    pub installer: bool,

    /// Emit a structured installer outcome on stdout (explicit choice required).
    #[arg(long = "installer-json", requires = "installer")]
    pub installer_json: bool,

    /// Select the invoking executable for installation.
    #[arg(
        long = "install-current",
        requires = "installer",
        conflicts_with = "install_version"
    )]
    pub install_current: bool,

    /// Select an exact published clud release for installation.
    #[arg(
        long = "install-version",
        value_name = "VERSION",
        requires = "installer",
        conflicts_with = "install_current"
    )]
    pub install_version: Option<String>,

    /// Install the executable without changing shell profiles or User PATH.
    #[arg(long = "no-modify-path", requires = "installer")]
    pub no_modify_path: bool,

    /// Issue #83: allow `--clean-worktrees` to remove dirty / unpushed
    /// worktrees. Locked worktrees are still preserved.
    #[arg(long = "force")]
    pub force: bool,

    #[arg(long = "experimental-daemon-centralized", hide = true)]
    pub experimental_daemon_centralized: bool,

    #[arg(long = "daemon-state-dir", hide = true)]
    pub daemon_state_dir: Option<PathBuf>,

    /// Issue #135: reserved for forward compatibility. The merged
    /// always-on clud daemon (`__daemon`) hosts both session ops and the
    /// GC registry, so the prior `--daemon=gc` / `--daemon=session`
    /// distinction is no longer required. Kept as an accepted flag so
    /// older clud invocations don't error.
    #[arg(long = "daemon", value_name = "MODE", hide = true)]
    pub daemon_mode: Option<String>,

    /// Issue #135: opt out of the GC daemon auto-spawn for this invocation.
    /// `clud gc *` operations fail fast with this flag because they
    /// require the daemon. Other code paths skip the spawn silently.
    #[arg(long = "no-daemon")]
    pub no_daemon: bool,

    /// Issue #340: keep env-tagged orphaned descendants alive on exit
    /// (skip the auto-reap; still prints the report unless --quiet-orphans).
    #[arg(long = "keep-orphans")]
    pub keep_orphans: bool,

    /// Issue #340: suppress the orphan-reaper surprise report on exit.
    /// Reaping still happens unless --keep-orphans is also set.
    #[arg(long = "quiet-orphans")]
    pub quiet_orphans: bool,

    /// Issue #340: dump the relevant env vars for each detected orphan
    /// alongside the report, to help author allowlist rules.
    #[arg(long = "explain-orphans")]
    pub explain_orphans: bool,

    /// Issue #466: suppress the foreground CPU-burn banner for this
    /// invocation. Banner is otherwise on by default; it polls the subtree
    /// CPU every 2 s and emits `[clud] cpu N % …` to stderr when subtree
    /// CPU crosses `max(50 %, 0.20 × num_cpus × 100 %)` for 3 sustained
    /// ticks. Permanent opt-out via `[foreground.cpu_banner] enabled =
    /// false` in `~/.clud/settings.json`.
    #[arg(long = "no-cpu-banner")]
    pub no_cpu_banner: bool,

    #[command(subcommand)]
    pub command: Option<Command>,

    #[arg(last = true, id = "BACKEND_ARGS")]
    pub passthrough: Vec<String>,

    /// API key typed after an API-key provider flag
    /// (`clud --deepseek <API_KEY>`), lifted out of `passthrough` at parse time
    /// so it is stored in the native vault instead of being sent to the model
    /// as the session's first prompt.
    #[arg(skip)]
    pub inline_api_key: Option<InlineApiKey>,

    /// Runtime Codex `-c` config overrides loaded from ~/.clud/settings.json.
    #[arg(skip)]
    pub codex_config_overrides: Vec<String>,

    /// Selection normalized once before bootstrap or credential access.
    #[arg(skip)]
    pub resolved_model_selection: Option<crate::provider_catalog::ResolvedModelSelection>,

    /// Original invocation, retained so desktop terminal launchers can hand every
    /// backend option to the child clud process without re-serializing clap's
    /// parsed representation.
    #[arg(skip)]
    pub raw_argv: Vec<String>,

    /// `-c N` / `--continue N`: continue the Nth newest session in this
    /// directory without the picker; `0` is the last save spot (#2405).
    #[arg(skip)]
    pub continue_index: Option<usize>,
}

impl std::fmt::Debug for Args {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Args")
            .field("claude", &self.claude)
            .field("codex", &self.codex)
            .field("deepseek", &self.deepseek)
            .field("kimi", &self.kimi)
            .field("openrouter", &self.openrouter)
            .field("unsafe_mode", &self.unsafe_mode)
            .field(
                "passthrough",
                &crate::secret_redaction::redact_args(&self.passthrough),
            )
            .field("inline_api_key", &self.inline_api_key)
            .field(
                "raw_argv",
                &crate::secret_redaction::redact_args(&self.raw_argv),
            )
            .finish_non_exhaustive()
    }
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
#[value(rename_all = "lower")]
pub enum WebTermPreference {
    On,
    Off,
}

impl WebTermPreference {
    pub const fn enabled(self) -> bool {
        matches!(self, Self::On)
    }
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
#[value(rename_all = "lower")]
pub enum OpenRouterZdrPreference {
    On,
    Off,
}

impl OpenRouterZdrPreference {
    pub const fn enabled(self) -> bool {
        matches!(self, Self::On)
    }
}

impl Args {
    /// Return the first clud option whose semantics are owned by Claude or
    /// Codex and therefore cannot be translated honestly to DeepSeek Harness.
    /// Backend passthrough remains available after `--` for native `dsh`
    /// options.
    /// `allow_model` is true for an OpenRouter launch, where clud selects the
    /// model through its dsh overlay (#1829).
    pub fn unsupported_deepseek_harness_option(&self, allow_model: bool) -> Option<&'static str> {
        if self.message.is_some() {
            Some("--message")
        } else if self.continue_session {
            Some("--continue")
        } else if self.resume.is_some() {
            Some("--resume")
        } else if self.model.is_some() && !allow_model {
            Some("--model")
        } else if self.effort.is_some() {
            Some("--effort")
        } else if self.context_window.is_some() {
            Some("--context-window")
        } else if self.safe {
            Some("--safe")
        } else if self.unattended {
            Some("--unattended")
        } else if self.allow_plan_mode {
            Some("--allow-plan-mode")
        } else {
            None
        }
    }

    /// Return only provider intent that came from the command line. Model
    /// inference and saved defaults are resolved later so source metadata does
    /// not accidentally label `--provider` as a global setting.
    /// Lift an API key out of the backend argv when an API-key provider
    /// (`--deepseek`, `--kimi`, `--openrouter`, or `--provider` naming one)
    /// was selected. Before this, `clud --deepseek <API_KEY>` forwarded the
    /// key to Claude Code as its opening prompt: with no stored key the
    /// preflight then asked for one and the typed key was never used (the
    /// common case on fresh Windows installs), and with a stored key the key
    /// was sent to the model.
    pub fn extract_inline_api_key(&mut self) {
        let Some(provider) = self.explicit_model_provider() else {
            return;
        };
        if crate::provider_registry::descriptor_for(provider).is_none() {
            return;
        }
        if let Some(index) = self
            .passthrough
            .iter()
            .position(|token| looks_like_api_key(token))
        {
            let key = self.passthrough.remove(index);
            self.inline_api_key = Some(InlineApiKey::new(key.trim()));
        }
    }

    pub fn explicit_model_provider(&self) -> Option<ModelProvider> {
        if self.deepseek {
            Some(ModelProvider::DeepSeek)
        } else if self.kimi {
            Some(ModelProvider::Kimi)
        } else if self.openrouter {
            Some(ModelProvider::OpenRouter)
        } else if self.codex {
            Some(ModelProvider::Codex)
        } else if self.claude {
            Some(ModelProvider::Claude)
        } else {
            self.provider
        }
    }

    pub fn routing_mode(&self) -> RoutingMode {
        if self.unified || self.mode.as_deref() == Some("unified") {
            RoutingMode::Unified
        } else {
            RoutingMode::Direct
        }
    }

    /// `run` is an explicit spelling of the historical command-less launch.
    /// Erase it before orchestration so every bare-launch gate (stdin, setup,
    /// warnings, daemon dispatch, and command construction) sees one shape.
    pub fn normalize_explicit_run(&mut self) {
        if matches!(self.command, Some(Command::Run)) {
            self.command = None;
        }
    }

    /// The launch-time model allowlist (#1257), derived once from `--model`
    /// and `--allow-model`.
    ///
    /// With neither flag the *previous* model selection -- whatever
    /// `resolved_model_selection` resolved from saved settings or the catalog
    /// default -- is the only pin: a launch that names no model still runs
    /// inside the boundary it already selected, instead of leaving every slot
    /// and the discovery catalog open. Empty means nothing resolved at all,
    /// which is the only unconstrained case.
    pub fn model_allowlist(&self) -> Vec<String> {
        let explicit =
            crate::provider_catalog::model_allowlist(self.model.as_deref(), &self.allow_model);
        if !explicit.is_empty() {
            return explicit;
        }
        self.previous_model_selection_pin()
    }

    /// The id pinned when the user named no model (#1257): the resolved
    /// selection's wire id -- the id actually billed -- falling back to its
    /// CLI id. Empty when nothing resolved, which leaves the launch
    /// unconstrained.
    fn previous_model_selection_pin(&self) -> Vec<String> {
        self.resolved_model_selection
            .as_ref()
            .and_then(|selection| {
                selection
                    .wire_model
                    .as_deref()
                    .or(selection.model.as_deref())
            })
            .map(|id| vec![id.to_string()])
            .unwrap_or_default()
    }

    /// True when the pin came from the previous model selection rather than
    /// from the command line (#1257). The runtime reports exactly this case
    /// as a green startup line, because the user did not ask for it.
    pub fn model_pin_is_from_previous_selection(&self) -> bool {
        self.model.is_none() && self.allow_model.is_empty() && !self.model_allowlist().is_empty()
    }

    /// Give `--allow-model` without `--model` a main model. The allowlist
    /// constrains every slot the launch can use, so it must also name the one
    /// the main conversation starts on -- otherwise the launch's own first
    /// turn would be outside its own boundary. Runs before provider inference
    /// and selection resolution so every later consumer sees one pin.
    pub fn normalize_model_allowlist(&mut self) {
        if self.model.is_none() && !self.allow_model.is_empty() {
            self.model = Some(self.allow_model[0].trim().to_string());
        }
    }
}

#[derive(Subcommand, Debug, Clone)]
pub enum Command {
    /// Serve Claude or Codex and optional OpenRouter models to local API clients.
    Serve {
        /// Enable the Claude upstream.
        #[arg(long, required_unless_present = "codex")]
        claude: bool,
        /// Enable Codex using clud's managed login or OPENAI_API_KEY.
        #[arg(long)]
        codex: bool,
        /// Add the configured OpenRouter upstream.
        #[arg(long)]
        openrouter: bool,
        /// Local port for API clients; zero chooses an available port.
        #[arg(long, default_value_t = 8317)]
        port: u16,
    },
    /// Acquire a credential in a masked window and deliver it directly to a sink.
    Secret(crate::secret_prompt::Options),
    /// Explicit compatibility spelling for a normal backend launch.
    Run,
    /// Query clud's cached OpenRouter model and pricing catalog.
    Models {
        #[command(subcommand)]
        subcommand: ModelsSubcommand,
    },
    /// Install or update Codex through CLUD's verified standalone-installer path.
    CodexUpdate,
    /// Install or confirm DeepSeek Harness in clud's private npm prefix.
    DshUpdate,
    /// Report hook and alias health: stale or retired aliases in clud's own
    /// alias dirs, a foreign `clud-cmd-scan` first on PATH, and the hook
    /// matcher's tool coverage (#1954). Exits 1 when anything is wrong.
    Doctor {
        /// Print the report as one JSON document.
        #[arg(long)]
        json: bool,
    },
    /// Run a CI job locally under bosn and show only the verdict and the
    /// failing steps' diagnostics (#1839).
    Ci {
        /// Workflow job id, e.g. `test-linux-x64-unit`; omit for the PR plan.
        job: Option<String>,
        /// Also print log lines matching this regex (e.g. a test module).
        #[arg(long)]
        filter: Option<String>,
        /// Workflow file, when not `.github/workflows/ci.yml`.
        #[arg(long)]
        workflow: Option<String>,
        /// Machine-readable output.
        #[arg(long)]
        json: bool,
    },
    /// Manage provider credentials. Claude authentication remains owned by
    /// Claude Code and is reported as externally managed.
    Auth {
        #[command(subcommand)]
        subcommand: Option<AuthSubcommand>,
    },
    /// Deprecated compatibility alias for `clud auth <action> codex`.
    #[command(name = "codex-auth", hide = true)]
    CodexAuth {
        #[command(subcommand)]
        subcommand: CodexAuthSubcommand,
    },
    /// Deprecated compatibility alias for `clud auth <action> deepseek`.
    #[command(name = "deepseek-auth", hide = true)]
    DeepseekAuth {
        #[command(subcommand)]
        subcommand: DeepseekAuthSubcommand,
    },
    Loop {
        /// Prompt text, path to a local file, or a GH issue/PR URL.
        task: Option<String>,
        #[arg(long = "loop-count", default_value = "50")]
        loop_count: u32,
        /// Force re-fetch of a cached GH issue/PR body.
        #[arg(long = "refresh")]
        refresh: bool,
        /// Do not inject the DONE/BLOCKED marker contract into the prompt.
        #[arg(long = "no-done", alias = "no-done-marker", conflicts_with = "done")]
        no_done: bool,
        /// Re-enable the DONE/BLOCKED contract using a custom DONE marker path.
        #[arg(long = "done", conflicts_with = "no_done")]
        done: Option<String>,
        /// Re-run the loop after it completes, sleeping for the given duration
        /// between runs (for example `30s`, `5m`, `1h`).
        #[arg(long = "repeat")]
        repeat: Option<String>,
    },
    Up {
        #[arg(short = 'm', long = "message")]
        message: Option<String>,
        #[arg(long = "publish")]
        publish: bool,
    },
    Rebase,
    Fix {
        url: Option<String>,
    },
    /// Implement a URL or free-form task end-to-end under the `/goal` contract.
    /// With no target, prompt for one in an interactive foreground terminal.
    Do {
        target: Option<String>,
    },
    /// Grind the current repo's issues page with `/loop`.
    // It requires the Claude harness's native interactive `/loop`; clud never
    // emulates it with external relaunches, markers, a turn cap, or a headless
    // prompt path. See docs/architecture/grind.md.
    ///
    /// With no argument, resolves the `origin` remote and maps it to the
    /// forge's issues page (`<repo>/issues` for GitHub, `<repo>/-/issues`
    /// for GitLab); errors if the remote is neither. An explicit URL is
    /// used verbatim, exactly like a URL passed to `clud do`.
    ///
    /// `clud grind reconcile` instead runs the feature-branch reconcile pass
    /// (#1393) in the current checkout and exits with its status.
    Grind {
        url: Option<String>,
    },
    /// Edit videos with browser-use/video-use (#1851). Loads the pinned
    /// video-use skill as a plugin for this one Claude session only; ordinary
    /// `clud` sessions never see it.
    Video {
        /// Directory holding the source videos (default: current directory).
        dir: Option<std::path::PathBuf>,
        /// Reinstall the pinned video-use checkout and its Python deps, then exit.
        #[arg(long)]
        update: bool,
        /// Print the pinned video-use checkout path (installing it first if
        /// missing), then exit. The explicit-only `/video-use` skill uses it.
        #[arg(long, conflicts_with = "update")]
        path: bool,
    },
    /// Install clud's bundled skills, agent types and workflows now, the same
    /// files a launch installs. `--home` targets another home directory; the
    /// real-harness tests (#1323) use it to populate an isolated config.
    /// Print the `/do` prompt for a target in this checkout: the starting-
    /// branch verdict and the contract (#1322). The `/do` skill's body runs
    /// this at invocation, so its output is what the model sees.
    #[command(hide = true)]
    DoPrompt {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        target: Vec<String>,
    },
    /// Print the repo's detected `./lint` / `./test` scripts for the
    /// `/grind` router (#1336). The router's body runs this at invocation.
    #[command(hide = true)]
    GrindScripts,
    /// Safely remove paths by moving them to the clud trash. Accepts rm-style
    /// flags and limits deletion to this session's allowed locations (#1461).
    #[command(name = "safe-rm", disable_help_flag = true)]
    SafeRm {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Print or clear this session's `/grind` run-facts file (#1337). The
    /// router writes its run facts there; clud's hook reads them by the
    /// payload's session id.
    #[command(hide = true)]
    GrindFacts {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    #[command(hide = true)]
    InstallAssets {
        #[arg(long = "home", value_name = "DIR")]
        home: Option<std::path::PathBuf>,
    },
    Wasm {
        module: String,
        #[arg(long = "invoke", default_value = "run")]
        invoke: String,
    },
    Attach {
        session_id: Option<String>,
        #[arg(long = "last", short = 'l')]
        last: bool,
    },
    Kill {
        session_id: Option<String>,
        #[arg(long = "all")]
        all: bool,
    },
    /// Kill all active background sessions.
    Slay,
    List,
    /// Inspect or manage a live checkout claim.
    Claim {
        #[command(subcommand)]
        action: ClaimSubcommand,
    },
    /// Inspect daemon-sampled CPU/RSS process trees.
    Top {
        /// Emit machine-readable JSON.
        #[arg(long = "json")]
        json: bool,
        /// Print one snapshot and exit.
        #[arg(long = "once")]
        once: bool,
        /// Keep refreshing. With `--json`, prints one compact JSON object per line.
        #[arg(long = "watch", conflicts_with = "once")]
        watch: bool,
        /// Render rows as a process tree. This is the default text mode.
        #[arg(long = "tree", conflicts_with = "flat")]
        tree: bool,
        /// Render rows as one flat sorted table.
        #[arg(long = "flat", conflicts_with = "tree")]
        flat: bool,
        /// Sort key for tree siblings and flat rows.
        #[arg(long = "sort", value_enum, default_value_t = TopSort::Cpu)]
        sort: TopSort,
        /// Cap displayed rows per subtree in tree mode, or total rows in flat mode.
        #[arg(long = "limit", default_value_t = 20)]
        limit: usize,
        /// Include dead PIDs sampled within this duration, e.g. `30s`, `5m`.
        #[arg(long = "since", value_name = "DURATION")]
        since: Option<String>,
        /// Restrict output to one cohort, e.g. `CLUD:71584` or `71584`.
        #[arg(long = "originator", value_name = "CLUD:PID")]
        originator: Option<String>,
    },
    /// pm2-style log viewer: dump or tail a session's captured output.
    ///
    /// With no session id, lists all sessions that have log files and prints
    /// the last line of each. With an id, prints the log (last `--lines` or
    /// all) and optionally keeps following new output via `--follow`.
    /// `--last` resolves to the most-recently-created session (live or
    /// exited), mirroring `clud attach --last`. Read-only: never takes
    /// exclusive ownership of the session and never evicts attached clients.
    Logs {
        session_id: Option<String>,
        /// Keep watching the file and print new output as it arrives. Exits
        /// once the session has terminated (after printing a status line).
        #[arg(long = "follow", short = 'f')]
        follow: bool,
        /// Print only the last N lines from the file. Default: all.
        #[arg(long = "lines", short = 'n')]
        lines: Option<usize>,
        /// Operate on the most recently created session (live or exited).
        #[arg(long = "last", short = 'l', conflicts_with = "session_id")]
        last: bool,
    },
    /// Issue #110: tracked-entry garbage collection (redb-backed
    /// registry at `~/.clud/data.redb`).
    ///
    /// Subcommands: `list`, `prune`, `purge`, `all`, `reconcile`. Running
    /// `clud gc` with no subcommand prints this help summary.
    Gc {
        #[command(subcommand)]
        subcommand: Option<GcSubcommand>,
    },
    /// Inspect or edit clud settings.
    ///
    /// Running `clud config` with no subcommand prints this help summary.
    Config {
        #[command(subcommand)]
        subcommand: Option<ConfigSubcommand>,
    },
    /// Manage the trust allowlist for foreign checkouts' hooks
    /// (zackees/clud#967 Phase 4, #966 D9).
    ///
    /// An `extern` checkout's own hooks stay off until this command records
    /// the allow in the parent's gitignored `.clud/settings.local.json`,
    /// keyed by the checkout's name and origin URL, so a re-clone from a
    /// different remote does not inherit the trust. Running `clud extern`
    /// with no subcommand prints this help summary.
    Extern {
        #[command(subcommand)]
        subcommand: Option<ExternSubcommand>,
    },
    /// Issue #183: open the local web dashboard served by the always-on
    /// clud daemon. Shows live sessions, garbage tracking, and the repos
    /// clud has been launched in. Loopback only.
    Ui {
        /// Print `/state.json` to stdout and exit without launching a browser.
        #[arg(long = "json")]
        json: bool,
        /// Print the dashboard URL and ensure the daemon is up, but do
        /// not launch a browser. Handy when running on a headless host.
        #[arg(long = "no-open")]
        no_open: bool,
    },
    /// Quarantine paths under ~/.clud/trash and let daemon GC reap them.
    Trash {
        /// Allow copy + best-effort source removal when source and trash
        /// live on different volumes.
        #[arg(long = "cross-volume")]
        cross_volume: bool,
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<PathBuf>,
    },
    /// Run and inspect bundled clud tools without starting the daemon.
    Tool {
        #[command(subcommand)]
        subcommand: ToolSubcommand,
    },
    /// Issue #469 (beta prototype): POST one telemetry event to the
    /// always-on clud daemon's HTTP server. Captures parent PID, time,
    /// the `cmd` string passed in, the current working directory, and
    /// every env var beginning with `CLUD_`. The daemon URL is read
    /// from `$CLUD_DAEMON_HTTP_SERVER`. By default missing env / unreachable
    /// daemon are silent (exit 0) so a hook caller is never broken; with
    /// `--fail-on-no-server` either failure causes a non-zero exit so
    /// tests can prove a real round-trip.
    Log {
        /// Free-form command string describing what the caller was doing.
        /// Stored verbatim in the telemetry record.
        #[arg(long = "cmd", short = 'c')]
        cmd: String,
        /// Exit non-zero if `CLUD_DAEMON_HTTP_SERVER` is unset OR the
        /// POST fails. Without this flag, failures are swallowed.
        #[arg(long = "fail-on-no-server")]
        fail_on_no_server: bool,
    },
    /// Install and persist fast local tooling defaults.
    Optimize {
        /// Toolchain family to optimize. Defaults to Rust.
        #[arg(value_enum, default_value_t = OptimizeTarget::Rust)]
        target: OptimizeTarget,
        /// Persist the recommendation in ~/.clud/settings.json.
        #[arg(long = "global", conflicts_with = "repo")]
        global: bool,
        /// Write a repo-local .clud/settings.json directive.
        #[arg(long = "repo", conflicts_with = "global")]
        repo: bool,
        /// Install soldr if it is missing from PATH.
        #[arg(
            long = "install-soldr",
            default_value_t = true,
            action = ArgAction::Set,
            num_args = 0..=1,
            default_missing_value = "true",
            value_parser = clap::value_parser!(bool),
        )]
        install_soldr: bool,
        /// Enable soldr shims for future clud-managed Rust setup.
        #[arg(
            long = "use-soldr-shims",
            default_value_t = true,
            action = ArgAction::Set,
            num_args = 0..=1,
            default_missing_value = "true",
            value_parser = clap::value_parser!(bool),
        )]
        use_soldr_shims: bool,
        /// Ignored with a warning: soldr cannot be pinned and always follows latest.
        #[arg(long = "soldr-version", default_value = "latest")]
        soldr_version: String,
    },
    /// Control the always-on clud daemon.
    Daemon {
        #[command(subcommand)]
        subcommand: DaemonSubcommand,
    },
    /// Inspect or verify crash-report symbolication (#374 PR 3/3).
    ///
    /// clud builds with `debug = "line-tables-only"` embed every line
    /// table in the binary itself, so there are no sidecar files to
    /// install. This subcommand is an opportunistic verifier that
    /// confirms the running binary can symbolicate recent crash reports
    /// in `~/.clud/state/crashes/`. `clud symbols` (bare) prints a
    /// summary; `clud symbols install` and `clud symbols verify` exit 1
    /// when any inspected report is unsymbolicated.
    Symbols {
        #[command(subcommand)]
        subcommand: Option<SymbolsSubcommand>,
    },
    /// Record and report per-bucket test runtimes for this checkout (#407).
    ///
    /// `clud test run --bucket unit -- <cmd>` wraps a test command, records
    /// what it cost, and returns the child's exit code unchanged.
    /// `clud test stats` reports p50/p90/n per bucket so the run-all-vs-
    /// targeted choice is made from data. Store is `.clud/test-runtime/`,
    /// per-checkout and gitignored.
    Test {
        #[command(subcommand)]
        subcommand: TestSubcommand,
    },
    /// Interactively toggle global clud settings (zackees/clud).
    ///
    /// Drops into a small cross-platform TUI checkbox menu over
    /// `~/.clud/settings.json` (created with defaults on first use, same as
    /// every other `clud_settings` consumer). Space toggles, q quits and
    /// prompts to save if anything changed.
    #[command(args_conflicts_with_subcommands = true)]
    Settings {
        /// Print current settings and exit; no TUI, no raw terminal mode.
        #[arg(long = "list")]
        list: bool,
        /// Propose one settings change for approval in a native desktop window.
        #[command(subcommand)]
        change: Option<crate::clud_settings::authorized::Mutation>,
    },
    /// Internal (#1189): print clud's toast inside Claude Code's status line.
    /// Claude Code runs this as the injected `statusLine` command.
    #[command(name = "statusline", hide = true)]
    Statusline {
        #[arg(long = "session-pid")]
        session_pid: u32,
        #[arg(long = "state-dir")]
        state_dir: PathBuf,
        /// The user's own status-line command, base64url, run first.
        #[arg(long = "chain-b64")]
        chain_b64: Option<String>,
    },
    /// Inspect the current agent session (#2122).
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
    /// Internal (#922): Claude lifecycle hook that keeps clud's per-cwd
    /// session index current. Claude Code runs this; it reads the hook
    /// payload on stdin and never fails the session.
    #[command(name = "session-hook", hide = true)]
    SessionHook {
        #[arg(long = "event")]
        event: String,
        /// The launch's route: a provider name, or `unified`.
        #[arg(long = "route")]
        route: String,
        #[arg(long = "state-dir")]
        state_dir: PathBuf,
        /// Portable recovery context to inject at `SessionStart`.
        #[arg(long = "recovery-file")]
        recovery_file: Option<PathBuf>,
    },
    /// Open the explicitly requested persistent Command Prompt.
    #[command(name = "__start-cmd", hide = true)]
    InternalStartCmd,
    /// Test seam for the OpenAI Chat Completions ingress: read one
    /// OpenAI-shaped request body on stdin, write the translated Messages
    /// request body to stdout. With `--response` it runs the other direction
    /// instead (Anthropic Messages response in, OpenAI completion out).
    /// Never used by a launched session.
    #[command(name = "__openai-chat-translate", hide = true)]
    InternalOpenAiChatTranslate {
        /// Translate an Anthropic Messages response, not an OpenAI request.
        #[arg(long)]
        response: bool,
        /// Read the response as SSE and emit one chat.completion.chunk per line.
        #[arg(long, requires = "response")]
        stream: bool,
        /// Model to report when the response body carries none.
        #[arg(long = "for-model")]
        for_model: Option<String>,
        /// Emit the exact HTTP body the route writes, not one JSON per line:
        /// a chat.completion body, or `data: <chunk>` frames plus `[DONE]`.
        #[arg(long, requires = "response")]
        http: bool,
    },
    #[command(name = "__gemini-client-translate", hide = true)]
    InternalGeminiClientTranslate {
        /// Model to serve the request with; without it the request's own model wins.
        #[arg(long)]
        model: Option<String>,
        /// Translate an Anthropic Messages response into a Gemini response.
        #[arg(long)]
        response: bool,
        /// Read the response as SSE and emit one Gemini response chunk per line.
        #[arg(long)]
        stream: bool,
        /// Emit the exact HTTP body a route writes, not one JSON per line.
        #[arg(long, requires = "response")]
        http: bool,
    },
    #[command(name = "__daemon", hide = true)]
    InternalDaemon {
        #[arg(long = "state-dir")]
        state_dir: PathBuf,
    },
    #[command(name = "__worker", hide = true)]
    InternalWorker {
        #[arg(long = "state-dir")]
        state_dir: PathBuf,
        #[arg(long = "session-id")]
        session_id: String,
        #[arg(long = "daemon-pid")]
        daemon_pid: u32,
        #[arg(long = "spec-file")]
        spec_file: PathBuf,
    },
}

#[derive(Subcommand, Debug, Clone)]
pub enum ClaimSubcommand {
    /// Claim the current checkout for this live clud session.
    Acquire,
    /// Release this session's claim without affecting another session.
    ReleaseOwn,
    /// Show live sessions and their claims in the current checkout.
    Who,
    /// Release a claim for a checkout after interactive confirmation.
    Release {
        checkout: std::path::PathBuf,
        #[arg(long)]
        yes: bool,
    },
}

/// Credential providers managed through `clud auth`.
#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
#[value(rename_all = "lower")]
pub enum AuthProvider {
    Codex,
    Deepseek,
    Kimi,
    Openrouter,
    Claude,
}

#[derive(Subcommand, Debug, Clone)]
pub enum ModelsSubcommand {
    /// List the lowest-priced eligible programming models.
    Cheapest {
        /// Emit stable machine-readable JSON.
        #[arg(long = "json")]
        json: bool,
    },
}

impl AuthProvider {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Deepseek => "deepseek",
            Self::Kimi => "kimi",
            Self::Openrouter => "openrouter",
            Self::Claude => "claude",
        }
    }
}

/// Action-first credential commands under `clud auth`.
#[derive(Subcommand, Debug, Clone)]
pub enum AuthSubcommand {
    /// Sign in to a provider managed by clud.
    Login {
        #[arg(value_enum)]
        provider: AuthProvider,
        /// Required only for Codex subscription authentication.
        #[arg(long = "acknowledge-experimental")]
        acknowledge_experimental: bool,
        /// Do not open the Codex authorization URL in a browser.
        #[arg(long = "no-browser")]
        no_browser: bool,
    },
    /// Show secret-free status for all providers or one provider.
    Status {
        #[arg(value_enum)]
        provider: Option<AuthProvider>,
        /// Emit stable JSON for automation.
        #[arg(long = "json")]
        json: bool,
    },
    /// Remove only credentials owned by clud.
    Logout {
        #[arg(value_enum)]
        provider: AuthProvider,
        /// Emit stable JSON for automation.
        #[arg(long = "json")]
        json: bool,
    },
}

/// Subcommands under the deprecated `clud codex-auth` alias. See `codex_auth.rs`.
#[derive(Subcommand, Debug, Clone)]
pub enum CodexAuthSubcommand {
    /// Start the experimental ChatGPT subscription sign-in flow.
    Login {
        /// Required acknowledgement that subscription compatibility is
        /// experimental and may change independently of clud.
        #[arg(long = "acknowledge-experimental")]
        acknowledge_experimental: bool,
        /// Do not open a browser automatically; print the URL instead.
        #[arg(long = "no-browser")]
        no_browser: bool,
    },
    /// Show the clud-managed subscription login state without secrets.
    Status {
        /// Emit stable JSON for automation.
        #[arg(long = "json")]
        json: bool,
    },
    /// Remove only clud-managed subscription credentials.
    Logout {
        /// Emit stable JSON for automation.
        #[arg(long = "json")]
        json: bool,
    },
}

/// Subcommands shared by every vault-backed Anthropic-compat provider's
/// auth commands (`provider_auth::run_for`) -- originally DeepSeek-only,
/// now also used by Kimi (#937 Phase 3). Named generically for that reason.
#[derive(Subcommand, Debug, Clone)]
pub enum ApiKeyAuthSubcommand {
    /// Prompt for and store the provider's API key in the native credential vault.
    Login,
    /// Report whether an API key is available without revealing it.
    Status {
        /// Emit stable JSON for automation.
        #[arg(long = "json")]
        json: bool,
    },
    /// Remove the clud-managed API key from the native credential vault.
    Logout {
        /// Emit stable JSON for automation.
        #[arg(long = "json")]
        json: bool,
    },
}

/// Compatibility alias: the CLI-visible `deepseek-auth` subcommand name and
/// its flags are unchanged; only the underlying subcommand type is now
/// shared across vault-backed Anthropic-compat providers. Keeps
/// `Command::DeepseekAuth` and every existing external reference to this
/// type name compiling unchanged.
pub type DeepseekAuthSubcommand = ApiKeyAuthSubcommand;

impl Command {
    /// The clap name of this subcommand if it is one of the hidden internal
    /// process roles, else `None`.
    ///
    /// Exists for [`crate::runtime_cache::role_pid_is_load_bearing`] (#333):
    /// the daemon and worker have their PIDs recorded by *other* processes, so
    /// they must not take the runtime-cache re-exec hop, which cannot preserve
    /// a PID on Windows. Returning the clap name rather than a bool keeps the
    /// policy in `runtime_cache` next to the reason for it.
    pub fn internal_name(&self) -> Option<&'static str> {
        match self {
            Self::InternalStartCmd => Some("__start-cmd"),
            Self::InternalDaemon { .. } => Some("__daemon"),
            Self::InternalWorker { .. } => Some("__worker"),
            _ => None,
        }
    }
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptimizeTarget {
    #[value(alias = "soldr")]
    Rust,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopSort {
    Cpu,
    Mem,
    Rss,
    Age,
}

/// Subcommands under `clud config`.
#[derive(Subcommand, Debug, Clone)]
pub enum ConfigSubcommand {
    /// Print global, local, and merged settings.
    Show {
        /// Emit machine-readable JSON.
        #[arg(long = "json")]
        json: bool,
    },
    /// Open a settings file in an editor.
    Edit {
        /// Edit repo-local .clud/settings.json instead of ~/.clud/settings.json.
        #[arg(long = "local")]
        local: bool,
        /// Editor command to run, e.g. `code --wait`.
        #[arg(long = "editor", value_name = "CMD")]
        editor: Option<String>,
    },
}

/// Subcommands under `clud test`. See `crates/clud-bin/src/test_runtime/`.
#[derive(Subcommand, Debug, Clone)]
pub enum TestSubcommand {
    /// Run a test command, recording its duration and the pre-run CPU load.
    Run {
        /// Bucket this run belongs to: unit | integration | e2e | smoke.
        #[arg(long = "bucket", default_value = "unit")]
        bucket: String,
        /// Optional test filter/target, recorded alongside the duration so a
        /// future query can localize "which one is slow".
        #[arg(long = "target")]
        target: Option<String>,
        /// The command to run, after `--`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Capture test output for the bounded report tool.
    #[command(hide = true)]
    Capture {
        /// Directory verified by the host-specific privacy implementation.
        #[arg(long)]
        output_dir: std::path::PathBuf,
        /// Maximum time to wait for the child and its output writers.
        #[arg(long, default_value_t = 3600)]
        timeout_secs: u64,
        /// Command to execute, after `--`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Report per-bucket runtime statistics.
    Stats {
        /// Limit the report to one bucket.
        #[arg(long = "bucket")]
        bucket: Option<String>,
        /// Machine-readable output for agent consumption.
        #[arg(long = "json")]
        json: bool,
    },
}

/// Subcommands under `clud symbols`. See `crates/clud-bin/src/symbols.rs`.
#[derive(Subcommand, Debug, Clone)]
pub enum SymbolsSubcommand {
    /// Verify that the running binary's embedded line tables resolve
    /// recent crash report backtraces. With the embed-everywhere
    /// strategy, this is a no-op when symbols are already present and
    /// a diagnostic when they're not. Exits 1 if any inspected report
    /// is unsymbolicated.
    Install,
    /// Same as `install` but with an explicit `--all` toggle.
    Verify {
        /// Verify every report under `~/.clud/state/crashes/` rather
        /// than just the most-recent one.
        #[arg(long = "all")]
        all: bool,
    },
}

/// Subcommands under `clud daemon`. See `crates/clud-bin/src/daemon/`.
#[derive(Subcommand, Debug, Clone)]
pub enum DaemonSubcommand {
    /// Print the private loopback API discovery document.
    #[command(name = "api-info")]
    ApiInfo {
        #[arg(long = "json")]
        json: bool,
    },
    /// Restart the daemon process so the next CLI call uses the current binary.
    Restart,
    /// Stop the daemon if it is running without starting a replacement.
    Stop,
    /// Print the current running-process adoption preview.
    #[command(name = "running-process", alias = "servicedef")]
    RunningProcess {
        /// Emit machine-readable JSON.
        #[arg(long = "json")]
        json: bool,
    },
    /// Report the last orphan-sweep result and freshness (#465). Exits
    /// non-zero if no sweep has run within 2× the sweep interval.
    #[command(name = "orphan-status")]
    OrphanStatus {
        /// Emit machine-readable JSON.
        #[arg(long = "json")]
        json: bool,
    },
}

/// Subcommands under `clud tool`. See `crates/clud-bin/src/tool_run.rs`.
#[derive(Subcommand, Debug, Clone)]
pub enum ToolSubcommand {
    /// Invoke a bundled tool by its `~/.clud/tools/`-relative path,
    /// forwarding any trailing args to the tool. Example:
    /// `clud tool run github/pr_merge_watch.py 404`.
    /// A resumable watchdog timeout returns 124 with `status: in-progress`
    /// on stderr; invoke the same tool again to keep watching. Exit 0 means
    /// the tool itself completed successfully.
    Run {
        /// Path under `~/.clud/tools/` (e.g. `github/pr_merge_watch.py`).
        rel_path: String,
        /// Arguments forwarded verbatim to the tool. Use `--` to pass flags
        /// the clud parser would otherwise consume.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// List tool invocations in the current clud session — slice 3 of #427.
    List {
        /// Emit a JSON array instead of the human-readable table.
        #[arg(long = "json")]
        json: bool,
        /// Show the long-form `<session-pid>-<tool-id>` ID in the table.
        #[arg(long = "long")]
        long: bool,
    },
    /// Query the full JSONL log of one invocation with optional filters
    /// — slice 4 of #427.
    Log {
        /// Reference to the invocation. Same forms as `tool info`.
        reference: Option<String>,
        /// Look up by the tool's own OS PID instead of session-local id.
        #[arg(long = "pid")]
        pid: Option<u32>,
        /// Which stream to read: `stdout`, `stderr`, or `combined` (default).
        #[arg(long = "stream", default_value = "combined")]
        stream: String,
        /// Only entries newer than `now - <duration>` (e.g. `5m`, `1h`).
        #[arg(long = "since")]
        since: Option<String>,
        /// Only entries older than `now - <duration>`.
        #[arg(long = "until")]
        until: Option<String>,
        /// Absolute time range as two integer epoch-ms values.
        #[arg(long = "between", number_of_values = 2)]
        between: Option<Vec<String>>,
        /// Substring match on the decoded line text.
        #[arg(long = "grep")]
        grep: Option<String>,
        /// Show only the first N matching entries.
        #[arg(long = "head")]
        head: Option<usize>,
        /// Show only the last N matching entries.
        #[arg(long = "tail")]
        tail: Option<usize>,
        /// Emit the raw JSONL stream instead of decoded text.
        #[arg(long = "json")]
        json: bool,
    },
    /// History of tool invocations matching optional filters — slice 4
    /// of #427.
    Ledger {
        /// Restrict to one tool name.
        #[arg(long = "tool")]
        tool: Option<String>,
        /// Session scope: `current` (default), `previous`, or `all`.
        #[arg(long = "session", default_value = "current")]
        session: String,
        /// Emit a JSON array instead of the human-readable table.
        #[arg(long = "json")]
        json: bool,
    },
    /// Show current state + last N lines of stdout/stderr for one
    /// invocation — slice 3 of #427.
    Info {
        /// Reference to the invocation. Accepts:
        /// * a bare session-local integer (`3`)
        /// * a long-form `<session-pid>-<tool-id>` (`47180-3`)
        /// * `@<tool-name>` or `@<tool-name>:N` for N-th-most-recent
        ///
        /// Omit to default to the most recently started invocation.
        reference: Option<String>,
        /// Look up by the tool's own OS PID instead of the session-local
        /// integer. Bare integers always mean tool-id, not PID — use this
        /// flag to disambiguate.
        #[arg(long = "pid")]
        pid: Option<u32>,
        /// Number of trailing stdout/stderr lines to show per stream.
        #[arg(long = "lines", default_value_t = 20)]
        lines: usize,
        /// Emit a JSON object instead of the human-readable view.
        #[arg(long = "json")]
        json: bool,
    },
}

/// Subcommands under `clud extern` (zackees/clud#967 Phase 4).
#[derive(Subcommand, Debug, Clone)]
pub enum ExternSubcommand {
    /// Trust an extern checkout so its own hooks fire for its files.
    ///
    /// The allow entry is recorded in the parent's gitignored
    /// `.clud/settings.local.json`, keyed by the checkout's directory name
    /// and its origin remote URL, so a re-clone from a different remote does
    /// not inherit the trust.
    Trust {
        /// The checkout's directory name under the repo's extern directory.
        name: Option<String>,
        /// Print the recorded allow entries (optionally only for `name`)
        /// and exit without changing anything.
        #[arg(long)]
        list: bool,
        /// Remove the allow entry for `name` instead of adding one.
        #[arg(long)]
        revoke: bool,
    },
}

/// Subcommands under `clud session` (#2122).
#[derive(Subcommand, Debug, Clone)]
pub enum SessionAction {
    /// The session's edit/lint/build/test/CI breakdown with Bash wall time,
    /// as clud's verify-ratio hook recorded it.
    Stats {
        /// Claude Code session id; defaults to `CLAUDE_CODE_SESSION_ID`.
        #[arg(long = "session")]
        session: Option<String>,
    },
}

/// Subcommands under `clud gc`. See `crates/clud-bin/src/gc/`.
#[derive(Subcommand, Debug, Clone)]
pub enum GcSubcommand {
    /// Print tracked entries, newest first.
    List {
        /// Issue #135: emit a JSON array instead of the human-readable table.
        #[arg(long = "json")]
        json: bool,
        /// Report daemon-cached Rust target sizes across repository worktrees.
        #[arg(long, conflicts_with = "kind")]
        targets: bool,
        /// Restrict to a single managed kind (e.g. `worktree`, `trash`).
        #[arg(long = "kind")]
        kind: Option<String>,
    },
    /// Drop stale/unreferenced entries for one managed kind, or `all`.
    Prune {
        /// Preview the removal plan without touching anything.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Only affect entries created longer ago than this duration
        /// (e.g. `2d`, `48h`). Overrides the per-kind default prune window.
        #[arg(long = "older-than", value_name = "DURATION")]
        older_than: Option<String>,
        /// Managed kind to prune (e.g. `worktree`, `uv-cache`, `trash`),
        /// or `all` for every managed kind (issue #506).
        #[arg(value_name = "KIND", conflicts_with = "kind")]
        kind_pos: Option<String>,
        /// Compatibility alias for the positional `KIND`.
        #[arg(long = "kind", value_name = "KIND")]
        kind: Option<String>,
    },
    /// Remove all entries for one managed kind, or `all`. Destructive;
    /// requires `--yes`.
    Purge {
        /// Preview the removal plan without touching anything.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Skip the interactive confirmation prompt.
        #[arg(long = "yes", short = 'y')]
        yes: bool,
        /// Only affect entries created longer ago than this duration
        /// (e.g. `2d`, `48h`). Without it, purge removes every entry.
        #[arg(long = "older-than", value_name = "DURATION")]
        older_than: Option<String>,
        /// Managed kind to purge (e.g. `worktree`, `uv-cache`, `trash`),
        /// or `all` for every managed kind (issue #506).
        #[arg(value_name = "KIND", conflicts_with = "kind")]
        kind_pos: Option<String>,
        /// Compatibility alias for the positional `KIND`.
        #[arg(long = "kind", value_name = "KIND")]
        kind: Option<String>,
    },
    /// Operate across every managed kind. Defaults to safe prune.
    All {
        /// Purge every managed kind instead of pruning stale entries.
        #[arg(long = "purge")]
        purge: bool,
        /// Preview the removal plan without touching anything.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Required with `--purge`.
        #[arg(long = "yes", short = 'y')]
        yes: bool,
        /// Only affect entries created longer ago than this duration
        /// (e.g. `2d`, `48h`).
        #[arg(long = "older-than", value_name = "DURATION")]
        older_than: Option<String>,
    },
    /// Walk `.claude/worktrees/` in the current repo and insert any
    /// previously-untracked worktree directories.
    Reconcile,
    /// Issue #2101: pin a worktree (default: the current directory) so the
    /// daemon never reclaims it. Bare `--list` prints the kept paths.
    Keep {
        /// Path to keep. Defaults to the current directory.
        #[arg(value_name = "PATH")]
        path: Option<std::path::PathBuf>,
        /// Release a previously kept path instead.
        #[arg(long = "remove")]
        remove: bool,
        /// Print the kept paths.
        #[arg(long = "list", conflicts_with_all = ["path", "remove"])]
        list: bool,
    },
}

const TOP_LEVEL_SUBCOMMANDS: &[&str] = &[
    "serve",
    "do-prompt",
    "grind-scripts",
    "grind-facts",
    "safe-rm",
    "install-assets",
    "loop",
    "up",
    "rebase",
    "fix",
    "do",
    "grind",
    "video",
    "wasm",
    "attach",
    "kill",
    "slay",
    "list",
    "claim",
    "top",
    "logs",
    "log",
    "gc",
    "config",
    "extern",
    "ui",
    "trash",
    "tool",
    "optimize",
    "daemon",
    "symbols",
    "settings",
    "test",
    "auth",
    "secret",
    "models",
    "codex-update",
    "dsh-update",
    "doctor",
    "ci",
    "codex-auth",
    "deepseek-auth",
    "run",
    "statusline",
    "session",
    "session-hook",
    "__daemon",
    "__worker",
    "__start-cmd",
    "__openai-chat-translate",
    "__gemini-client-translate",
];

impl Args {
    pub fn parse_with_passthrough() -> Self {
        let raw: Vec<String> = std::env::args().collect();
        Self::parse_from_raw(raw)
    }

    pub fn parse_from_raw(raw: Vec<String>) -> Self {
        let (normalized, continue_index) = extract_continue_index(&raw);
        let normalized = normalize_bare_resume_before_subcommand(&normalized);
        let normalized = normalize_known_option_dashes(&normalized);
        let normalized = match split_inline_key_assignments(&normalized) {
            Ok(normalized) => normalized,
            Err(message) => {
                use clap::CommandFactory;
                Args::command()
                    .error(clap::error::ErrorKind::InvalidValue, message)
                    .exit()
            }
        };
        let (known, unknown) = match split_known_unknown(&normalized) {
            Ok(parts) => parts,
            Err(message) => {
                use clap::CommandFactory;
                Args::command()
                    .error(clap::error::ErrorKind::UnknownArgument, message)
                    .exit()
            }
        };
        let mut args = Args::parse_from(known.clone());
        if let Some(Command::SafeRm { args: command_args }) = &mut args.command {
            // Clap consumes global options even after a trailing subcommand.
            // For this command, every token after its name belongs to its
            // native-style parser, including `--dry-run` and `--verbose`.
            if let Some(index) = known.iter().position(|part| part == "safe-rm") {
                *command_args = known[index + 1..].to_vec();
            }
        }
        if args
            .resume
            .as_ref()
            .is_some_and(|value| value.as_deref() == Some(""))
        {
            args.resume = Some(None);
        }
        args.passthrough.extend(unknown);
        if continue_index.is_some() {
            args.continue_session = true;
            args.continue_index = continue_index;
        }
        args.extract_inline_api_key();
        args.raw_argv = raw;
        args
    }
}

/// Only correct an exact, public clud option while still in the top-level
/// option region. In particular, never rewrite prompt values or backend argv.
fn normalize_known_option_dashes(raw: &[String]) -> Vec<String> {
    use clap::CommandFactory;
    let command = Args::command();
    let public_longs: std::collections::HashSet<&str> = command
        .get_arguments()
        .filter(|argument| !argument.is_hide_set())
        .filter_map(|argument| argument.get_long())
        .collect();
    let mut normalized = raw.to_vec();
    let mut index = 1;
    while index < raw.len() {
        let token = raw[index].as_str();
        if token == "--" || TOP_LEVEL_SUBCOMMANDS.contains(&token) {
            break;
        }
        let (name, assignment) = token
            .split_once('=')
            .map_or((token, None), |(name, value)| (name, Some(value)));
        let corrected = if name == "-deepseek" {
            Some("--deepseek".to_string())
        } else {
            let suffix = ["-–", "—", "–", "―", "−", "﹣", "－", "\u{00ad}"]
                .iter()
                .find_map(|prefix| name.strip_prefix(prefix));
            suffix.and_then(|suffix| public_longs.contains(suffix).then(|| format!("--{suffix}")))
        };
        let effective_name = corrected.as_deref().unwrap_or(name);
        let consumes_next = assignment.is_none()
            && (SPLITTER_VALUE_FLAGS.contains(&effective_name)
                || SPLITTER_SHORT_VALUE_FLAGS.contains(&effective_name));
        if let Some(corrected) = corrected {
            normalized[index] = match assignment {
                Some(value) => format!("{corrected}={value}"),
                None => corrected,
            };
        }
        if consumes_next {
            index += 2;
        } else {
            index += 1;
        }
    }
    normalized
}

/// `--resume` has an optional value, so clap would consume a following built-in
/// name (`--resume do`) as the session id and never parse the subcommand. Give
/// registered subcommands precedence by spelling a bare resume as an explicit
/// empty value for clap, then normalize that empty value back to `Some(None)`.
/// A session actually named like a subcommand remains addressable with
/// `--resume=<session>`. A following flag (`--resume --model opus`) is never a
/// session id either, so it gets the same treatment; otherwise the splitter
/// would take the flag as resume's value (#1899).
/// Lift a numeric value after `-c`/`--continue` (or `--continue=N`) out of
/// the argv (#2405). `-c` is a switch, so `clud -c 0` used to forward `0` to
/// the harness as the prompt. Only the top-level option region is scanned.
fn extract_continue_index(raw: &[String]) -> (Vec<String>, Option<usize>) {
    let mut out = Vec::with_capacity(raw.len());
    let mut index = None;
    let mut skip_value = false;
    let mut done = false;
    let mut i = 0;
    while i < raw.len() {
        let arg = raw[i].as_str();
        out.push(raw[i].clone());
        i += 1;
        if done || i == 1 {
            continue;
        }
        if std::mem::replace(&mut skip_value, takes_next_value(arg)) {
            continue;
        }
        if arg == "--" || TOP_LEVEL_SUBCOMMANDS.contains(&arg) {
            done = true;
            continue;
        }
        if let Some(value) = arg.strip_prefix("--continue=") {
            if let Ok(n) = value.parse::<usize>() {
                *out.last_mut().unwrap() = "--continue".to_string();
                index = Some(n);
            }
            continue;
        }
        if matches!(arg, "-c" | "--continue") {
            if let Some(n) = raw
                .get(i)
                .filter(|next| !next.is_empty() && next.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|next| next.parse::<usize>().ok())
            {
                index = Some(n);
                i += 1;
            }
        }
    }
    (out, index)
}

fn normalize_bare_resume_before_subcommand(raw: &[String]) -> Vec<String> {
    let mut normalized = raw.to_vec();
    let mut skip_value = false;
    for i in 1..raw.len() {
        let arg = raw[i].as_str();
        let consumes_next = takes_next_value(arg) && !matches!(arg, "--resume" | "-r");
        if std::mem::replace(&mut skip_value, consumes_next) {
            continue;
        }
        if arg == "--" || TOP_LEVEL_SUBCOMMANDS.contains(&arg) {
            break;
        }
        if matches!(arg, "--resume" | "-r")
            && raw.get(i + 1).is_some_and(|next| {
                TOP_LEVEL_SUBCOMMANDS.contains(&next.as_str())
                    || (next.starts_with('-') && next.len() > 1)
            })
        {
            normalized[i] = "--resume=".to_string();
        }
    }
    normalized
}

/// True when `arg` is a clud value flag spelled without `=`, so the next token
/// is its value rather than a flag or subcommand.
fn takes_next_value(arg: &str) -> bool {
    SPLITTER_VALUE_FLAGS.contains(&arg) || SPLITTER_SHORT_VALUE_FLAGS.contains(&arg)
}

/// Provider flags that accept an inline API key.
const INLINE_KEY_PROVIDER_FLAGS: &[&str] = &["--deepseek", "--kimi", "--openrouter"];

/// `--deepseek=<API_KEY>` must behave exactly like `--deepseek <API_KEY>`
/// (#1197). The provider flags are on/off switches, so the splitter never
/// matched the `=` form: it forwarded the whole token, key included, to the
/// harness as an unknown flag and launched plain Claude. A key-shaped value is
/// split into its own token, where [`Args::extract_inline_api_key`] lifts it
/// out. Any other value is an error rather than a passthrough token, so nothing
/// typed after `=` can reach the harness argv; the message never echoes it.
fn split_inline_key_assignments(raw: &[String]) -> Result<Vec<String>, String> {
    let mut normalized = Vec::with_capacity(raw.len() + 1);
    let mut in_clud_flags = true;
    let mut skip_value = false;
    for (index, arg) in raw.iter().enumerate() {
        // A value flag's value (`--name test`) is never a subcommand (#1900).
        let is_value = std::mem::replace(&mut skip_value, in_clud_flags && takes_next_value(arg));
        if index > 0 && !is_value && (arg == "--" || TOP_LEVEL_SUBCOMMANDS.contains(&arg.as_str()))
        {
            in_clud_flags = false;
            skip_value = false;
        }
        let assignment = arg.split_once('=').filter(|(flag, _)| {
            in_clud_flags && index > 0 && INLINE_KEY_PROVIDER_FLAGS.contains(flag)
        });
        match assignment {
            Some((flag, value)) if looks_like_api_key(value) => {
                normalized.push(flag.to_string());
                normalized.push(value.trim().to_string());
            }
            Some((flag, _)) => {
                return Err(format!(
                    "{flag} does not take a value; pass an API key as `{flag} <API_KEY>` or \
                     `{flag}=<API_KEY>` (keys start with `sk-`)"
                ));
            }
            None => normalized.push(arg.clone()),
        }
    }
    Ok(normalized)
}

const SPLITTER_VALUE_FLAGS: &[&str] = &[
    "--install-version",
    "--prompt",
    "--message",
    "--resume",
    "--resume-mode",
    "--model",
    "--allow-model",
    "--provider-only",
    "--provider",
    "--mode",
    "--failover",
    "--effort",
    "--context-window",
    "--harness",
    "--name",
    "--transcript",
    "--backlog-size",
    "--graphics",
    "--graphics-image",
    "--loop-count",
    "--done",
    "--repeat",
    "--daemon-state-dir",
    "--stale-after",
    // Issue: `clud gc prune/purge --older-than <dur>` value arg.
    "--older-than",
    "--daemon",
    "--state-dir",
    // Issue #469: `clud log --cmd "..."` arg.
    "--cmd",
    "--set-web-term",
    "--set-openrouter-zdr",
    "--reason",
    // #1189: hidden `clud statusline` arguments.
    "--session-pid",
    "--chain-b64",
];
const SPLITTER_SHORT_VALUE_FLAGS: &[&str] = &["-p", "-m", "-r"];

/// Flags that take an optional `=`-joined value: bare, they are bool flags
/// (and are listed in `bool_flags`); `--flag=value` carries the value.
const OPTIONAL_VALUE_FLAGS: &[&str] = &["--coauthor"];

#[expect(
    clippy::too_many_lines,
    reason = "complexity ratchet baseline (zackees/ci.yml#229); split this function"
)]
fn split_known_unknown(raw: &[String]) -> Result<(Vec<String>, Vec<String>), String> {
    let mut known = vec![raw[0].clone()];
    let mut unknown = Vec::new();
    let mut i = 1;

    let value_flags: &[&str] = SPLITTER_VALUE_FLAGS;
    let short_value_flags: &[&str] = SPLITTER_SHORT_VALUE_FLAGS;
    let bool_flags: &[&str] = &[
        "--zdr",
        "--no-data-collection",
        "--redact-secrets",
        "--no-redact-secrets",
        "--installer",
        "--installer-json",
        "--install-current",
        "--no-modify-path",
        "--continue",
        "--claude",
        "--codex",
        "--deepseek",
        "--kimi",
        "--openrouter",
        "--unified",
        "--failover-allow-metered",
        "--subprocess",
        "--pty",
        "--safe",
        "--no-land",
        "--unsafe",
        "--unattended",
        "--allow-plan-mode",
        "--coauthor",
        "--dry-run",
        "--detach",
        "--detachable",
        "--verbose",
        "--no-cpu-banner",
        "--experimental-daemon-centralized",
        "--all",
        "--last",
        "--refresh",
        "--no-done",
        "--no-done-marker",
        "--no-dnd",
        "--no-drag-drop",
        "--clean-worktrees",
        "--fix-hooks",
        "--no-fix-hooks",
        "--yes",
        "--force",
        "--no-daemon",
        "--keep-orphans",
        "--quiet-orphans",
        "--explain-orphans",
        "--json",
        "--no-open",
        "--demo-gfx-sixel",
        "--help",
        "--version",
        "--web-term",
        "--kitty-term",
        // Issue #469: `clud log --fail-on-no-server` bool flag.
        "--fail-on-no-server",
        // `clud settings --list` bool flag.
        "--list",
        // `clud extern trust --list` / `--revoke` bool flags.
        "--revoke",
    ];
    let short_bool_flags: &[&str] = &["-c", "-v", "-h", "-V", "-y"];
    // Subcommands whose own parser consumes `--` as data rather than as clud's
    // end-of-flags marker. See the `--` branch below (issue #508).
    //
    // This was a single constant while `tool run` was the only `trailing_var_arg`
    // parser in the CLI. `test run -- <cmd>` (#407) is the second, and a
    // subcommand missing from this list does not fail loudly: clud silently
    // swallows everything after `--` as backend passthrough, and the subcommand
    // sees an empty command vector.
    const SEPARATOR_OWNING_SUBCOMMANDS: &[&str] =
        &["tool", "test", "safe-rm", "secret", "grind-facts"];

    // Which subcommand we are inside, once one has been seen. `None` means the
    // tokens still belong to clud's own top-level flags.
    let mut subcommand: Option<&str> = None;

    while i < raw.len() {
        let arg = &raw[i];

        // Issue #508: who owns `--` depends on which subcommand we are inside.
        //
        // Normally it keeps its usual meaning — end clud's own flags, hand the
        // rest to the backend agent — which is what `clud loop task -- --verbose`
        // relies on.
        //
        // `clud tool run <tool> … -- <cmd…>` is the exception: there the
        // separator is *data* for the bundled tool, which does its own
        // `run -- <cmd…>` split. Diverting it into `passthrough`, which nothing
        // on the tool path ever reads, is what made the documented invocation
        // fail with `run: missing command`.
        //
        // Only the listed subcommands own raw argv. In particular, grind-facts
        // needs its task file list after `--` (#1897). Handing `--` to a
        // subcommand that does not expect it makes clap reject the unknown
        // flag and exit the process.
        if arg == "--"
            && !subcommand.is_some_and(|name| SEPARATOR_OWNING_SUBCOMMANDS.contains(&name))
        {
            unknown.extend_from_slice(&raw[i + 1..]);
            break;
        }

        if subcommand.is_some() {
            known.push(arg.clone());
            i += 1;
            continue;
        }

        if let Some(name) = TOP_LEVEL_SUBCOMMANDS
            .iter()
            .find(|name| *name == &arg.as_str())
        {
            known.push(arg.clone());
            subcommand = Some(name);
            i += 1;
            continue;
        }

        if bool_flags.contains(&arg.as_str()) || short_bool_flags.contains(&arg.as_str()) {
            known.push(arg.clone());
            i += 1;
            continue;
        }

        if arg.starts_with("--") {
            if let Some((prefix, _)) = arg.split_once('=') {
                // `--coauthor` is a bool flag whose optional value must be
                // `=`-joined (#1317), so its `=` form is clud's too.
                if value_flags.contains(&prefix) || OPTIONAL_VALUE_FLAGS.contains(&prefix) {
                    known.push(arg.clone());
                    i += 1;
                    continue;
                }
            }
        }

        if value_flags.contains(&arg.as_str()) || short_value_flags.contains(&arg.as_str()) {
            known.push(arg.clone());
            i += 1;
            if i < raw.len() {
                known.push(raw[i].clone());
            }
            i += 1;
            continue;
        }

        if !arg.starts_with('-') {
            if let Some(suggestion) = suggest_public_subcommand(arg) {
                eprintln!("[clud] note: '{arg}' is not a subcommand; did you mean '{suggestion}'?");
            }
        }
        validate_top_level_unknown(arg, raw.get(i + 1).map(String::as_str))?;
        unknown.push(arg.clone());
        i += 1;
    }

    Ok((known, unknown))
}

fn suggest_public_subcommand(token: &str) -> Option<String> {
    if token.chars().any(char::is_whitespace) || looks_like_api_key(token) {
        return None;
    }
    use clap::CommandFactory;
    let command = Args::command();
    let mut suggestions = clap::Command::default().name("clud");
    let visible: std::collections::HashSet<String> = command
        .get_subcommands()
        .filter(|subcommand| !subcommand.is_hide_set())
        .map(|subcommand| subcommand.get_name().to_string())
        .collect();
    for subcommand in command.get_subcommands() {
        if !subcommand.is_hide_set() {
            suggestions = suggestions.subcommand(subcommand.clone());
        }
    }
    suggestions
        .try_get_matches_from(["clud", token])
        .err()
        .and_then(|error| {
            error.to_string().lines().find_map(|line| {
                line.split_once("a similar subcommand exists: '")
                    .and_then(|(_, suffix)| {
                        suffix.split_once('\'').map(|(name, _)| name.to_string())
                    })
            })
        })
        .filter(|name| visible.contains(name))
}

fn validate_top_level_unknown(token: &str, following: Option<&str>) -> Result<(), String> {
    use clap::CommandFactory;
    let name = token.split_once('=').map_or(token, |(name, _)| name);
    let canonical = if let Some(name) = name.strip_prefix("--") {
        format!("--{name}")
    } else if let Some(name) = name.strip_prefix('-') {
        let name = name.strip_prefix('–').unwrap_or(name);
        format!("--{name}")
    } else if let Some(name) = ["—", "–", "―", "−", "﹣", "－", "\u{00ad}"]
        .iter()
        .find_map(|prefix| name.strip_prefix(prefix))
    {
        format!("--{name}")
    } else {
        return Ok(());
    };
    let command = Args::command();
    let all_options: std::collections::HashSet<String> = command
        .get_arguments()
        .filter_map(|argument| argument.get_long().map(|name| format!("--{name}")))
        .collect();
    let public_options: std::collections::HashSet<String> = command
        .get_arguments()
        .filter(|argument| !argument.is_hide_set())
        .filter_map(|argument| argument.get_long().map(|name| format!("--{name}")))
        .collect();
    if all_options.contains(&canonical) {
        return Err(format!(
            "clud's option splitter omitted {name}; this is a clud bug"
        ));
    }
    let suggestion = Args::command()
        .try_get_matches_from(["clud", canonical.as_str()])
        .err()
        .and_then(|error| {
            error.to_string().lines().find_map(|line| {
                line.split_once("a similar argument exists: '")
                    .and_then(|(_, suffix)| {
                        suffix
                            .split_once('\'')
                            .map(|(option, _)| option.to_string())
                    })
            })
        })
        .filter(|suggested| public_options.contains(suggested));
    let next_is_key = following.is_some_and(looks_like_api_key)
        || token
            .split_once('=')
            .is_some_and(|(_, value)| looks_like_api_key(value));
    if let Some(suggested) = suggestion {
        return Err(format!(
            "unexpected argument '{name}'; did you mean '{suggested}'? Pass backend arguments after --"
        ));
    }
    if next_is_key {
        return Err(format!(
            "unrecognized option '{name}' is followed by an API key; check the spelling or pass backend arguments after --"
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "args_tests.rs"]
mod tests;

#[cfg(test)]
mod grind_scripts_parse_tests {
    use super::*;

    #[test]
    fn grind_scripts_dispatches_as_subcommand_not_passthrough() {
        let raw: Vec<String> = ["clud", "grind-scripts"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let args = Args::parse_from_raw(raw);
        assert!(matches!(args.command, Some(Command::GrindScripts)));
        assert!(args.passthrough.is_empty());
    }

    #[test]
    fn claim_subcommands_are_not_backend_passthrough() {
        let args = Args::parse_from_raw(
            ["clud", "claim", "release", "/repo", "--yes"]
                .iter()
                .map(|value| value.to_string())
                .collect(),
        );
        assert!(args.passthrough.is_empty());
        assert!(matches!(
            args.command,
            Some(Command::Claim {
                action: ClaimSubcommand::Release { yes: true, .. }
            })
        ));
    }

    #[test]
    fn safe_rm_dispatches_with_raw_arguments() {
        let parse =
            |list: &[&str]| Args::parse_from_raw(list.iter().map(|s| s.to_string()).collect());
        let args = parse(&["clud", "safe-rm", "--purge", "a", "--", "-b"]);
        assert!(args.passthrough.is_empty());
        match args.command {
            Some(Command::SafeRm { args }) => {
                assert_eq!(args, vec!["--purge", "a", "--", "-b"]);
            }
            other => panic!("expected Command::SafeRm, got {other:?}"),
        }
        match parse(&["clud", "safe-rm", "--help"]).command {
            Some(Command::SafeRm { args }) => assert_eq!(args, vec!["--help"]),
            other => panic!("expected Command::SafeRm, got {other:?}"),
        }
        match parse(&["clud", "safe-rm", "--dry-run", "-v", "x"]).command {
            Some(Command::SafeRm { args }) => assert_eq!(args, vec!["--dry-run", "-v", "x"]),
            other => panic!("expected Command::SafeRm, got {other:?}"),
        }
    }

    #[test]
    fn grind_facts_dispatches_with_its_action() {
        let raw: Vec<String> = ["clud", "grind-facts", "path"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let args = Args::parse_from_raw(raw);
        assert!(args.passthrough.is_empty());
        match args.command {
            Some(Command::GrindFacts { args }) => assert_eq!(args, vec!["path".to_string()]),
            other => panic!("expected Command::GrindFacts, got {other:?}"),
        }
    }

    #[test]
    fn grind_facts_keeps_task_files_after_its_separator() {
        let task = [
            "task",
            "--checkout",
            "/checkout",
            "--",
            "src/a.rs",
            "-leading.rs",
        ];
        let raw = ["clud", "grind-facts"]
            .into_iter()
            .chain(task)
            .map(String::from)
            .collect();
        let parsed = Args::parse_from_raw(raw);
        assert!(parsed.passthrough.is_empty());
        match parsed.command {
            Some(Command::GrindFacts { args }) => assert_eq!(args, task),
            other => panic!("expected Command::GrindFacts, got {other:?}"),
        }
    }

    #[test]
    fn grind_reconcile_parses_as_grind_subcommand_not_passthrough() {
        let raw: Vec<String> = ["clud", "grind", "reconcile"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let args = Args::parse_from_raw(raw);
        match &args.command {
            Some(Command::Grind { url }) => assert_eq!(url.as_deref(), Some("reconcile")),
            other => panic!("expected Command::Grind, got {other:?}"),
        }
        assert!(args.passthrough.is_empty());
    }

    #[test]
    fn is_reconcile_matches_only_the_reconcile_keyword() {
        assert!(is_reconcile(Some("reconcile")));
        assert!(!is_reconcile(None));
        assert!(!is_reconcile(Some("https://github.com/o/r/issues/1")));
        assert!(!is_reconcile(Some("Reconcile")));
    }
}

/// True when `clud grind`'s positional argument selects the reconcile pass
/// (`clud grind reconcile`, #1393) rather than a goal URL.
pub fn is_reconcile(url: Option<&str>) -> bool {
    url == Some("reconcile")
}

/// A provider API key passed on the command line. `Debug` never prints it, so
/// verbose logging of [`Args`] cannot leak it.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct InlineApiKey(String);

impl InlineApiKey {
    pub fn new(key: &str) -> Self {
        Self(key.to_string())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for InlineApiKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InlineApiKey(<redacted>)")
    }
}

impl Drop for InlineApiKey {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.0.zeroize();
    }
}

/// Whether a bare argv token is an API key rather than a prompt. DeepSeek,
/// Kimi and OpenRouter keys all start `sk-` and contain no spaces; no
/// plausible opening prompt looks like that.
pub fn looks_like_api_key(token: &str) -> bool {
    let token = token.trim();
    token.len() >= 20
        && token.starts_with("sk-")
        && token[3..]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

# Toasts

In a Kitty graphics terminal with SGR mouse reporting, the upper-right gear
opens the authenticated daemon dashboard at `/processes/<clud-pid>/<start>`. The
dashboard filters the existing process sample by the launch's originator PID and start time,
including tagged descendants that now have a different OS parent. An unreaped
child appears as a zombie while it remains in the process table, then as
exited for up to a minute. The gear uses
mouse reports the child has enabled.

How clud shows short, expiring messages (a CPU episode, "cpu back to normal")
while a harness TUI owns the terminal. Introduced by #1189; the design choice
is recorded in [DD-071](../DESIGN_DECISIONS.md#dd-071-toasts-are-composited-inside-the-terminal-stream-not-drawn-in-an-external-window).

## The problem

Before #1189 the CPU banner (`cpu_banner.rs`) `eprintln!`ed `[clud] cpu …`
lines onto the same terminal the child (Claude Code, Codex) was drawing on.
Nothing positioned, erased, or expired them, and nothing serialised them with
the child's own redraws, so a shorter line landed over part of a longer one
and the child's footer repaint cleared the wrong rows.

## Model

`crates/clud-bin/src/toast/mod.rs`

- A **toast** is `(key, text, severity, expires_at?)`. Publishing an existing
  key replaces it in place; `expires_at = None` means the producer closes it.
- **`ToastBoard`** holds one toast per key and picks the visible one
  (highest severity, then most recent).
- **`ToastHub`** is the thread-safe board plus a version counter. Producers
  publish to it; the compositor polls the version on each writer wake-up. The
  hub lives for the launch while compositors are rebuilt per PTY iteration.
- **`ToastSink`** is what producers hold: `Hub` (PTY mode), `StatusFile`
  (subprocess mode), `Discard`, or `Recorder` (tests).

The CPU banner is the only producer today. `cpu_banner::toast_events` maps its
state machine to events: a live `cpu` toast refreshed every tick while an
episode runs, "cpu back to normal" expiring after 10 s, and a close when an
episode ends below the clear threshold.

## Surfaces

| Launch | Surface | Where it renders |
|---|---|---|
| PTY, kitty graphics terminal | **Kitty tier** | transient CPU HUD plus independent alert toasts over the top-right cells |
| PTY, other terminal, child on the alternate screen | **Text-cell tier** | styled cells, top-right row |
| PTY, other terminal, child on the main screen | **Fallback** | Claude: status line. Others: terminal title |
| Subprocess, Claude | **Status line** | Claude Code's `statusLine` row |
| Subprocess, other harness | none | toasts are dropped |

The Kitty tier reserves its separate top-right image placement for a `cpu`
event only while that event exists. Other toasts appear below it. Exact
provider-terminal token usage stays in Claude's status line, while the title
fallback retains model/cache health when no status line is drawn. The
conversation's usage snapshot remains available for those surfaces; it is never
rendered as a persistent Kitty strip.

Tier selection is `toast/tier.rs::decide`:

- kitty, Ghostty and WezTerm (probe, or `TERM`/`TERM_PROGRAM`/`KITTY_WINDOW_ID`/
  `WEZTERM_PANE`/`GHOSTTY_RESOURCES_DIR`) get the kitty tier;
- Konsole and iTerm2 report kitty graphics but implement it partially, so they
  stay on text cells until validated;
- Sixel-only terminals get text cells. Sixel adds nothing a text toast lacks:
  1-bit transparency, no blending over text, and the same repaint to remove;
- while clud's Sixel header owns a scroll region, in-grid tiers are off;
- `CLUD_TOAST_TIER=kitty|text|fallback|off` overrides the decision.

An interactive launch from a real terminal runs every harness through the PTY
pump on every platform (DD-086), so it gets in-grid toasts; headless `-p` and
`--subprocess` launches see the status line. On Windows, PTY sessions get the
kitty tier under WezTerm and text cells or the title elsewhere.

## Compositor

`crates/clud-bin/src/toast/compositor.rs`, run on the PTY pump's writer thread
(`session_output.rs::run_output_writer_composited`, extending DD-018's
reader/writer split). The writer channel carries `OutputMsg::Child` bursts and
`OutputMsg::Resize`; while the compositor is enabled, the writer wakes every
100 ms so a CPU event can first appear, fade, or expire even when the child
TUI emits no bytes.

For every child burst the compositor forwards the bytes, feeds a `vt100`
shadow of the child's intended screen and an `EscapeTracker`, then injects
toast bytes only when safe:

- **Between sequences.** `toast/tracker.rs` tracks CSI/OSC/DCS/APC/SOS/PM
  state and UTF-8 continuations; injecting mid-sequence corrupts both streams.
- **Outside synchronized updates.** Claude Code and Codex wrap frames in
  DECSET 2026; a block left open longer than 250 ms is drawn over anyway.
- **Not in pending-wrap.** With the cursor in the last column a CUP would
  cancel the terminal's pending wrap, so injection waits.
- **Cursor restored by CUP from the shadow**, never DECSC/DECRC (the child may
  own that single save slot). Origin mode is switched off for the toast and
  the restore is relative to the scroll region.
- **Every kitty command carries `q=2`**, or the terminal's reply would reach
  the child's stdin as typed input.

### Kitty tier

`toast/raster.rs` renders a rounded translucent panel, a severity accent bar,
the text (bundled DejaVu Sans Mono via `fontdue`, no system font library) and
a close X into a PNG at a nominal cell size; `toast/kitty.rs` uploads it once
(`a=t`) and places it with `c=`/`r=` so the terminal scales it to the real
cell grid. The placement is re-sent after every burst because images scroll
with text, and the image is re-uploaded after `ED 2`, `RIS` or an
alternate-screen switch, which drop images. Removal deletes the placement:
the child's cells were never touched, so nothing is repainted.

The CPU HUD has its own image and placement id. The keyed `cpu` toast is read
alongside the highest-priority non-CPU toast, so one cannot hide the other.
The HUD uses normal-toast-height text, has no close button, and is absent when
the CPU toast is absent. Its whole image is 90% opaque for the first 2 seconds
of an appearance, then 50% until the event closes or expires. Sample refreshes
and the brief recovery toast do not restart that clock. A hover overrides
opacity to 90% without changing content or lifetime. The image is re-pinned
after child bursts, re-uploaded after terminal image-dropping events, and
deleted when hidden or at session exit.

### CPU HUD lifetime and opt-out (#2367)

A CPU episode's live reading stays up for `cpu_banner::CPU_LIVE_VISIBLE`
(20 s), then the toast expires and later heartbeats of the same episode do not
re-show it, so a long burn stops covering the harness header. The "cpu back to
normal" notice still appears when the episode ends, and a new episode gets a
fresh window. To never show the overlay, pass `--no-cpu-banner` or set
`[foreground.cpu_banner] enabled = false` in clud settings. The HUD has no
close button or hotkey by design: it never consumes the child's input.

When Claude's status line is injected (`Fallback::StatusFile`, every Claude
PTY launch with a status writer), the compositor does not draw the CPU HUD at
all, in any tier: `Compositor::mirror_cpu_to_status` publishes the keyed `cpu`
toast to the status line instead. The reading therefore never covers header
text and needs no dismissal, including on Linux/kitty. Non-Claude harnesses
(title fallback) still get the grid HUD, which remains non-dismissable except
by the opt-outs above.

### CPU HUD hover

Hover is observed only when the child has already selected DECSET 1003
(any-motion) with SGR encoding. Entering the visible HUD raises opacity to
90%; leaving returns it to the timed value. clud never enables mouse tracking
itself and forwards clicks and motion over the CPU HUD byte-for-byte to the
child. Without any-motion reporting, the timed opacity behavior still works.
Only a normal toast's close button consumes a click and its release.

When no Kitty overlay can be drawn, the title fallback carries the effective
model and cache-health state. Claude's injected status line continues to show
the full exact token counters from the same launch snapshot.

### Text-cell tier

`toast/text_tier.rs` paints a one-row toast. Removal repaints the rectangle
from the shadow (ECH to true blanks, then `rows_formatted`), widened so it
never splits a wide character. Before each child burst the toast is lifted so
scrolls move the child's real content; if the burst begins mid-sequence the
lift is impossible and the whole screen is repainted afterwards. The tier is
limited to the alternate screen because on the main screen a scroll would
push toast cells into the terminal's native scrollback permanently.

### Click to dismiss

clud never enables mouse tracking itself; that would steal selection and
wheel scrollback from inline TUIs. When the shadow shows the child already
reports SGR mouse events (Claude Code fullscreen), the compositor publishes
the close button's rectangle and `toast/mouse.rs` filters stdin: a left press
inside it and its release are swallowed and dismiss the toast. Everything
else passes through byte-exact, and a held partial report (a lone Esc) is
released on the next idle poll.

## Claude status line

`crates/clud-bin/src/toast/statusline.rs`, wired by
`ForegroundRuntime::start_with_statusline`.

- `StatusStateWriter` keeps `<state_dir>/toasts/<pid>.json` in step with the
  session's toasts (`updated_ms`, text, severity, `expires_ms`) and removes it
  on drop.
- clud composes a `statusLine` whose command is
  `"<clud>" statusline --session-pid <pid> --state-dir "<dir>" [--chain-b64 …]`
  with `refreshInterval: 2` (never slower than the user's own).
- **The user's status line is chained, not replaced.** clud discovers the
  effective one (explicit `--settings`, then the project's
  `.claude/settings.local.json` / `settings.json`, then
  `~/.claude/settings.json`), base64url-encodes its command, and
  `clud statusline` runs it first with Claude's session JSON on stdin
  (`sh -c`; Git Bash on Windows), then appends the toast.
- **Windows runs the chain only under Git Bash (#1371).** The command is
  authored for Git Bash, which is where Claude Code runs it, so clud looks for
  Git Bash wherever Git for Windows puts it: `CLAUDE_CODE_GIT_BASH_PATH`, then
  `<Git>\bin\bash.exe` beside the `git.exe` on PATH (the installer puts only
  `Git\cmd` on PATH), then `%ProgramFiles%\Git\bin\bash.exe`, then any `bash`
  on PATH. With none found it never falls back to `cmd.exe`, whose quoting and
  `$VAR` rules would silently mis-run the command and leave an empty footer.
  Instead the user's line is replaced by a one-line notice naming the missing
  Git Bash and `CLAUDE_CODE_GIT_BASH_PATH`.
- The setting rides the same single launch-scoped `--settings` source hooks
  use: merged into that file, or into the user's own `--settings` document,
  which then replaces the argument.
- A non-expiring toast whose `updated_ms` is older than 90 s is ignored, so a
  crashed session cannot leave a stuck alert.
- When clud has observed a bridged provider terminal response, its exact
  ledger wins. Otherwise the callback incrementally scans the
  Claude transcript named by `transcript_path` and sibling `subagents/*.jsonl`.
  It counts each provider-reported `message.usage` once by hashed `message.id`,
  even when the transcript repeats a response or a sidechain appears in both
  locations. Offsets and opaque hashes live in a lock-guarded private cursor
  file; only aggregate counters and a public model label reach the atomic
  `<pid>.usage.json` snapshot. The PTY compositor reads that snapshot only for
  the title fallback; the Kitty HUD reads CPU events from `ToastHub`.
  The one-line display contains one cumulative read/write triple and the last
  completed model, with no per-call fallback. A malformed complete record or
  conflicting cross-file response makes exact accounting unprovable, so the
  callback withholds the cumulative triple and renders only the model. A
  partial trailing record waits for its newline. Both ledgers cover one
  conversation: a new main `transcript_path` starts a fresh cursor file and
  deletes the snapshot, and the bridge's `SessionStart`/`clear` hook zeroes
  its totals and bumps a usage generation the writer uses to refuse delayed
  pre-clear snapshots
  ([DD-186](../DESIGN_DECISIONS.md#dd-186-status-line-usage-is-scoped-to-one-conversation)).
  Native Codex
  without the Claude status-line callback requires bridge-observed usage.
- **Effort segment (#1464).** After the user's line and the usage line,
  `clud statusline` prints `effort: <level>`, bold orange for `high`,
  `xhigh` and `max`. The main session's level comes from, in order: the
  stdin JSON's `effort.level` (Claude Code's documented field, live across
  `/effort`, absent when the model has no effort parameter), then
  `CLAUDE_EFFORT`, then the most recent main-session request the gateway saw.
  `modelSettings.<model>.effortLevel` is not read. With no source the line is
  unchanged. **Per-agent counts need clud's gateway or bridge in the path**:
  each `POST /v1/messages` records its `output_config.effort` (and whether it
  carried `x-claude-code-agent-id`) in the state file's `efforts` list, kept
  to the last 10 minutes and 256 entries. Subagent requests ranked above the
  session's level render as `effort: medium · agents: 3×high`. A plain Claude
  backend without the gateway shows only the main-session level.

`clud statusline` is dispatched first thing in `main.rs`, before any daemon,
runtime-cache or launch work, because Claude runs it every couple of seconds.

## Configuration

```json
{ "foreground": { "toasts": { "enabled": true, "claude_statusline": true } } }
```

- `enabled = false`: no compositor, no status-line injection.
- `claude_statusline = false`: keep in-grid toasts, never compose a
  `statusLine` (restores the pre-#1189 Claude argv).
- Toasts are off whenever the CPU banner is off (`--no-cpu-banner`,
  `[foreground.cpu_banner] enabled = false`, `--dry-run`, detached launches).
- `CLUD_TOAST_TIER` forces a tier; `CLUD_TOAST_DEMO=<text>` (with optional
  `CLUD_TOAST_DEMO_SECS`) shows a toast at launch for manual validation.

## Limits

- `vt100` does not model strikethrough, underline style/colour, overline or
  OSC 8, so a text-tier repaint drops those inside the toast rectangle.
- Feeding the shadow costs a pass over every child byte while toasts are
  enabled in PTY mode.
- Konsole and iTerm2 kitty-graphics support is unvalidated (text tier).
- Daemon-attached sessions do not composite toasts.
- An external overlay window anchored to the terminal was evaluated and
  deferred (DD-071).

## Tests

| Layer | Where |
|---|---|
| Model, tracker, kitty encoding, raster, text tier, mouse, tier matrix, status line | `src/toast/*` unit tests (all CI lanes) |
| Compositor byte streams (every tier, deferral, origin mode, dismiss, resize) | `src/toast/compositor_tests.rs` |
| CPU HUD timing, separate placement, coexistence with a toast, redraw and cleanup | `src/toast/compositor_tests.rs` |
| CPU hover observation, split reports, byte-exact mouse pass-through | `src/toast/mouse.rs` unit tests |
| Banner never writes to the terminal; banner → toast events | `src/cpu_banner_tests.rs` |
| Status-line injection into Claude settings | `src/foreground_runtime.rs` tests |
| Real PTY sessions: kitty tier, title fallback (Linux, macOS, Windows), alternate-screen text tier (Unix) | `tests/integration/pty/toast_pty.rs` |

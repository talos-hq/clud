# Computer use

clud ships computer use built in: the [Talos HQ Computer Use](https://github.com/talos-hq/computer-use)
release
registered as an MCP server named `cua`, so the agent sees `mcp__cua__*`
tools (windows, screenshots, clicks, typing, real browsers) with no setup and
no permission prompts. Rationale:
[DD-233](../DESIGN_DECISIONS.md#dd-233-computer-use-is-built-in-through-a-pinned-cua-driver-provisioned-on-first-use).

Code: [`crates/clud-bin/src/computer_use/`](../../crates/clud-bin/src/computer_use/mod.rs).
Skill: [`clud-computer-use`](../../crates/clud-bin/assets/skills/clud-computer-use/SKILL.md).

## Registration

`command::builder::claude_mcp_servers` adds two tokens to a Claude launch:

- `--mcp-config={"mcpServers":{"cua":{"command":"<clud>","args":["computer-use","mcp"],"env":{"CLUD_CUA_PERMISSION_MODE":"<mode>"},"alwaysLoad":false}}}`
- `--allowedTools=mcp__cua`

and appends a two-sentence directive naming the skill. `--dry-run` shows both
tokens. The driver exposes ~65 tools whose schemas total ~220 KB; Claude Code
keeps MCP tools behind tool search, and `alwaysLoad:false` keeps even tools
the driver marks "load upfront" deferred, so an unused server costs only the
directive.

| Launch | Registered |
|---|---|
| Claude harness, Anthropic provider, direct route: interactive, `clud loop`, `clud grind` | yes, by default |
| the same route, headless one-shot (`clud -p`) | only with `--computer-use` |
| Gateway providers (OpenRouter, Kimi, unified), Codex/DeepSeek on the Claude harness | only with `--computer-use` |
| Codex harness | only with `--computer-use` (`-c mcp_servers.cua.*`, `default_tools_approval_mode="approve"`, `startup_timeout_sec=180`) |
| any launch with `--no-computer-use` | no |

A headless one-shot stays off because `claude -p` waits for every pending MCP
server before its first turn, which would add the driver's start (and, once,
its download) to every scripted call. Non-default routes stay off because a gateway that rejects tool search makes
Claude Code load every schema upfront on every request, and Codex loads them
all.

## Permissions

There are three layers, and none prompts:

1. **Harness.** `--allowedTools=mcp__cua` allows every cua tool in every mode,
   including `--safe` (DD-002's opt-out keeps prompts for Bash/Edit; it does
   not prompt per desktop action).
2. **cua-driver's authorization profile**, fixed at driver start from
   `CLUD_CUA_PERMISSION_MODE`:
   - default (YOLO) launch: `unrestricted`
     (`CUA_DRIVER_PERMISSION_MODE=unrestricted` +
     `CUA_DRIVER_DANGEROUSLY_BYPASS_APPROVALS=1`): no cua approval checks;
   - `--safe`: cua's `standard` profile. Observation, input, isolated
     browsers and validated file transfer still run silently; attaching to a
     logged-in browser profile, killing foreign processes and input into OS
     security prompts are refused, never asked.
   Any other value of the variable maps to `standard`, so a garbled or
   inherited value can only narrow.
   On macOS `cua-driver mcp` proxies to the signed `ComputerUse.app` daemon
   that LaunchServices starts. cua documents the environment form for Linux
   and Windows only, and this has not been verified on macOS: if the variables
   do not reach the daemon it runs cua's `standard` profile (still
   prompt-free; residual boundaries are refused). `--direct` would apply the
   mode in-process but attribute TCC to the terminal instead of ComputerUse, so
   clud does not use it.
3. **OS.** macOS TCC (Accessibility, Screen Recording) cannot be granted
   silently; see [macOS](#macos).

## Provisioning

`clud computer-use mcp` resolves the driver on every start
(`provision::resolve`), first match wins:

1. `CLUD_CUA_DRIVER=<path>`: used as-is; a path that is not a file is an
   error, never silently replaced.
2. The managed copy `~/.clud/libexec/cua-driver/<version>-<sha16>/`, accepted
   only when its `.clud-pin` marker names the pinned archive digest and the
   binary still hashes to the digest recorded at unpack time.

Another product's copy, such as one Hermes installed, is never reused: only
setup and install may read Hermes' tree or environment (the boundary test in
`setup_command/hermes_tests.rs`, #2609), and this resolution runs on the MCP
serve path. Point `CLUD_CUA_DRIVER` at it to share one copy deliberately.

Otherwise clud unpacks the archive embedded in its release binary. The Linux
GNU x64 and Windows x64 wheels use a first-use download to meet release size gates.
Development builds use the same path. clud first asks its uv runtime for the
platform wheel from PyPI at the exact `clud-computer-use` version. If uv cannot
resolve it, clud fetches the same archive from GitHub. The pin defines each
archive's URL, size, and SHA-256. clud verifies the archive before extraction and publishes
the installed tree atomically. A concurrent first use verifies the winner.

The archive is unpacked when the MCP server starts or when the user runs
`clud computer-use install`. After installing, reconnect the server with
`/mcp` or restart the session.

macOS uses the directory archive: only it carries the signed `ComputerUse.app`
whose identity (`com.taloshq.computer-use`) holds the TCC grants.

## Driver environment

`session_env::child_env` builds the child's full environment:

- the parent environment minus every variable whose name contains `API_KEY`,
  `TOKEN`, `SECRET`, `PASSWORD`, `CREDENTIAL` or `PRIVATE_KEY`;
- the permission mode above;
- `CUA_DRIVER_RS_TELEMETRY_ENABLED=0` unless the user set it;
- `CUA_DRIVER_RS_UPDATE_CHECK=0`, because clud owns the version.

On Linux it also repairs the display:

- **Stale `XAUTHORITY`.** A terminal that outlived a re-login keeps the old
  cookie path; every X11 call then fails with "Authorization required" and
  `list_windows` returns `[]`. When the inherited path does not exist, the
  newest `xauth_*` (KDE) or `.mutter-Xwaylandauth.*` (GNOME) file in
  `XDG_RUNTIME_DIR` is used.
- **Wayland.** Under a Wayland session clud sets
  `CUA_DRIVER_RS_ENABLE_WAYLAND=1` (cua-driver's native backend) unless
  `CLUD_CUA_NATIVE_WAYLAND=0`. Native mode lists native and XWayland windows;
  XWayland-only mode lists only XWayland ones.

## Platform support

| Session | Windows / AT-SPI tree | Window screenshot | Input into another app |
|---|---|---|---|
| X11 | yes | yes | yes |
| Wayland, XWayland app | yes | yes | yes |
| Wayland, native app | yes (compositor + AT-SPI) | KWin: after the user runs `cua-driver kwin-capture on`; otherwise `surface_identity_unproven` | semantic AT-SPI actions only; pixel input is refused (`background_unavailable` / `foreground_unavailable`) |
| no display | — | — | `browser_*` only, or run under Xvfb |
| macOS | yes after TCC grant | yes after TCC grant | yes |
| Windows | interactive session only (not Session 0) | yes | yes |

Desktop-scope screenshots on Wayland may raise a portal approval dialog that
the user answers. The skill tells the agent to report these limits instead of
retrying, and to prefer the `browser_*` tools (CDP) for web work, which do not
depend on the display server.

## macOS

macOS requires the user to approve Accessibility and Screen Recording for
ComputerUse once. clud asks **once per machine**: the first `computer-use mcp`
start on macOS writes `~/.clud/state/computer-use/tcc-grant-requested`, then
runs `cua-driver permissions grant` in the background (dialogs are attributed
to ComputerUse through LaunchServices). The marker is written first, so a
declined or interrupted request is never raised again automatically.
`clud computer-use grant` requests it again on demand.

## Commands

| Command | Does |
|---|---|
| `clud computer-use status [--json]` | pinned version and target, driver path and source, display kind, XAUTHORITY repair, native Wayland, permission modes, platform notes |
| `clud computer-use install` | provision now |
| `clud computer-use doctor` | `cua-driver doctor` with clud's environment |
| `clud computer-use grant` | macOS TCC request (no-op elsewhere) |
| `clud computer-use mcp` (hidden) | the stdio MCP server: provision, then `cua-driver mcp` with inherited stdio |

`mcp` writes only the driver's protocol to stdout; clud's own messages go to
stderr (Claude Code's MCP log).

## Bumping the pin

Edit every row in `computer_use/pin.rs` from the new release's `SHA256SUMS`
and asset sizes (`gh release view computer-use-v<v> -R talos-hq/computer-use --json
assets`), run `clud computer-use install` and the Xvfb proof below, and check
`cua-driver manifest` still advertises `mcp` and the permission variables.

## Tests

- Unit (`computer_use/tests.rs`): pin table shape and URL allowlist,
  provisioning decision (missing, installed, tampered, wrong size/digest,
  wrapped archive, zip, override, broken override), permission
  mapping, credential scrubbing, display detection, XAUTHORITY repair,
  Wayland opt-out, Claude and Codex registration payloads.
- Launch (`command/tests/computer_use_launch.rs`): default, `--safe`,
  `--no-computer-use`, gateway routes, Codex opt-in, argv parsing.
- Skill (`skills_tests.rs`): bundled, automatically invocable after install,
  names the server, recovery commands and platform limits.
- End to end (`tests/integration/test_computer_use_xvfb.py`, Linux, opt-in
  with `CLUD_TEST_CUA_E2E=1`): under Xvfb,
  the server clud registers in `--dry-run` answers `tools/list`, finds a real
  X11 window with `list_windows` and returns a screenshot.

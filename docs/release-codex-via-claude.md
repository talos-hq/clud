# Codex-via-Claude release checklist

This feature is opt-in: `clud --codex --harness claude`. Native Claude and
native Codex launches keep their existing defaults. Roll back a single launch
with `--harness default`, or reset the saved harness choice in `clud settings`.

Release orchestration and manual recovery follow
[automatic version tags](architecture/ci.md#automatic-version-tags-2764).
Complete this feature smoke record before merging the real version bump.

## Automated evidence (2026-08-04)

- Rust component coverage exercises loopback authentication, routing, bounded
  requests, fragmented SSE, backpressure, cancellation, no-replay behavior,
  shutdown, redaction, and credential refresh locking.
- The Windows credential-store suite verified a protected owner-only DACL and
  atomic replacement locally.
- The release-mode local bridge benchmark completed 200 synthetic full-path
  requests in 1286 ms (155.48 requests/s), with 1,712,128 bytes reported RSS
  growth on Windows x86_64. This is an observation, not a portable threshold.
- CI builds shipped artifacts for the supported target matrix; the bridge adds
  no Go, Node, npm, sidecar, or target-runner compiler dependency.

## Required release-manager smoke record

Do this with throwaway accounts/projects and record only date, OS/architecture,
Claude Code version, API/client version, and pass/fail outcome. Never paste a
token, bearer, account ID, bridge URL, callback URL, or log containing one.

| Scenario | Required outcome | Date/version/outcome |
|---|---|---|
| Platform API key, streamed text/tool/reasoning/cancel, foreground | One clean turn; no secret in terminal/log | _pending_ |
| Platform API key, daemon/detach and repeat | Lifecycle and pinned choices work | _pending_ |
| Experimental subscription login/status/refresh/tool/logout/re-login | Explicit source only; logout removes clud record | _pending_ |
| Native `clud`, `--claude`, `--codex` | No cross-route behavior/regression | _pending_ |
| Sticky session/global choice and CLI precedence | Green override notice and reset work | _pending_ |
| Release-artifact smoke on supported OS/architecture | Artifact runs without external runtime | _pending_ |

Do not advertise a broader compatibility claim until every row is recorded.

## Operator troubleshooting

- Missing `claude` executable: install it or use native Codex with
  `--harness default`.
- Unsupported provider/harness pair: only Codex-through-Claude is supported.
- Expired subscription: run `clud codex-auth login --acknowledge-experimental`.
- Callback ports: free 1455/1457 and retry.
- Bridge/upstream/proxy failure: inspect `--dry-run`, proxy/firewall policy,
  and the sanitized status; never publish bridge logs containing secrets.
- Clean removal: `clud codex-auth logout`, then reset harness settings if a
  global override was selected.

The source/license review is in [third-party-notices.md](third-party-notices.md).

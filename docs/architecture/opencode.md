# OpenCode harness implementation and acceptance plan

Status: implemented; the real-client mock acceptance suite passes.
Repository regression checks and PR landing remain required.

OpenCode is a harness, independent of the selected model provider. `--harness opencode` selects `HarnessSelection::OpenCode` and
`Backend::OpenCode`, retaining existing provider flags and defaults. Do not introduce an OpenCode model provider.

## Usage

```sh
clud --harness opencode --deepseek -p "Inspect this repository"
clud --harness opencode --claude --safe
clud --harness opencode --claude --resume=ses_previous -p "Continue"
```

Provider billing consent still applies. Set the repository's normal
`CLUD_ALLOW_METERED=1` and matching `CLUD_METERED_PROVIDER` when intentionally
using a metered provider. Dry-run stays credential-free.

For a served endpoint, set `ANTHROPIC_BASE_URL` to its origin (an existing
`/v1` suffix is also accepted), and supply its client token in the selected
provider's key environment variable: `ANTHROPIC_API_KEY` for Claude or
`DEEPSEEK_API_KEY` for DeepSeek. A custom endpoint skips the real provider's
credential probe so that a server client token is never sent to that provider.
The gateway continues to hold and forward its own upstream key.

Bare Claude/OpenCode launches default to `claude-sonnet-5-5`. The Claude Code
aliases `sonnet`, `opus`, `haiku`, and `fable` map to reviewed API IDs because
the Messages API does not accept those frontend aliases. The reviewed source
is [Anthropic's models overview](https://platform.claude.com/docs/en/models/overview)
(2026-11-02). Explicit full wire IDs remain unchanged. Managed daemon API
turns still require Claude or Codex; OpenCode supports normal foreground and
worker launch plans and its JSONL session identity is recognized.

## Launch contract

Discover `opencode` on PATH. Report an actionable installation error if absent;
never substitute another harness. Register it with the installed-harness picker,
settings serialization, bootstrap, route-plan renderer and daemon adapters.

A bare launch uses the interactive `opencode` command. A prompt launch uses
`opencode run --format json <prompt>`; translate clud's `-p` rather than passing
it through (OpenCode uses `-p` for the server password). Continue/resume map to
`--continue` and `--session`. Model selection uses `--model provider/model`.
Unsafe launches use the reviewed CLI's `--auto`; `--safe` omits it and preserves
OpenCode's permission prompts and explicit denies. Reject unsupported harness
options before spawn instead of silently dropping them. `grind` remains subject
to its native interactive loop requirement.

Use child-local `OPENCODE_CONFIG_CONTENT` for provider configuration; preserve
unrelated user config. OpenCode documents this as the highest precedence config
source. Do not write user auth/config files. Configure a custom provider with
an explicit SDK adapter and base URL for clud serve; never infer the wire
protocol from a URL or use a real provider as a failed-test fallback. The
OpenCode client receives the serve client token, never upstream credentials.
Provider keys and client tokens must be absent from dry-run output and logs.

Native DeepSeek and OpenRouter routes need explicit model and credential
mapping. Claude/Codex routes must either have a tested supported adapter or
fail explicitly. Unified routing must be rejected until its route contract is
implemented for OpenCode. Do not accidentally run Claude-specific runtime setup,
hooks, slash commands or stream rendering for this harness.

## Acceptance checks

1. Rust tests cover selection/settings round trips, unsupported routes,
   executable discovery, argv translation, safe mode, resume and model mapping.
2. Python subprocess tests run clud against a recording OpenCode executable to
   verify argv and child environment, missing executable failures, dry-run
   secrecy and unchanged existing harness behavior.
3. Run the real pinned OpenCode binary against `clud serve` backed by the
   existing isolated mock provider infrastructure. Assert a streamed text
   response reaches OpenCode and the upstream receives the selected model.
4. Exercise a complete tool cycle: mock upstream requests a real harmless file
   read, OpenCode executes it, the next request contains its result, and the
   final response reaches the client. Assert request count and tool-result
   content; process exit alone does not prove this.
5. Verify invalid client authentication and credential isolation at the served
   gateway. Mock tests use isolated configuration and explicit loopback URLs.
6. Exercise two independent clud serve processes in sequence with OpenCode as
   the client, checking that routing/auth survive both hops. A live DeepSeek
   smoke test is additional evidence when authorized credentials are available;
   deterministic mock coverage must remain runnable without paid credentials.
7. Run repository lint and relevant regression tests, complete pre-push review,
   and land through the required hosted checks and merge queue.

Record command versions and actual outcomes with the implementation. The real-client tests must continue to prove both streamed text and the
complete tool cycle; a successful process exit alone is insufficient.

## Reviewed upstream interface

On 2026-11-02, npm reports `opencode-ai@1.19.5`. The installed binary reports
`1.19.5`; `opencode run --help` confirms `--format json`, `--model`, `--continue`,
`--session`, `--auto` and the conflicting `-p` password option.

Primary references: [configuration](https://opencode.ai/docs/config/),
[providers](https://opencode.ai/docs/providers/), and
[CLI](https://opencode.ai/docs/cli/). Recheck the actual pinned CLI when
implementing flags; upstream documentation changes independently of releases.

## Evidence collected

The real-client acceptance test is
`tests/integration/test_opencode_client.py`. With OpenCode 1.19.5 and the
current debug clud binary, it passed on 2026-11-02 in 12.52 seconds:

```sh
CLUD_INTEGRATION_TESTS=1 \
CLUD_TEST_BINARY=/path/to/debug/clud \
CLUD_TEST_OPENCODE_BINARY=/path/to/opencode \
python -m pytest tests/integration/test_opencode_client.py -q
```

It verifies a streamed tool request, an actual OpenCode file read, the canary
in the follow-up tool result, final response delivery, model identity and
client-token isolation. The existing `test_serve_gateway.py` suite also passed
all 10 cases in 91.73 seconds. These establish client/gateway compatibility;
the expanded run below verifies the clud harness launch path as well.

The integration upstream override is all-or-nothing: Codex, Anthropic,
DeepSeek and OpenRouter override URLs must all be present. The acceptance
fixture points all four at its loopback mock, even though it enables only the
Claude route. Missing override variables caused the initial failed smoke
attempts; keeping the complete override prevents a partial mock configuration
from selecting a default upstream.


The expanded acceptance run passed **12 tests in 51.31 seconds** using the
rebuilt feature binary. It includes the recording-harness tests in
`tests/integration/test_opencode_harness.py`, plus real OpenCode 1.19.5 through:

- Direct OpenCode -> clud serve -> Claude mock.
- clud OpenCode launch -> clud serve -> Claude mock.
- clud OpenCode launch -> clud serve -> DeepSeek mock.
- clud OpenCode launch -> clud serve -> clud serve -> DeepSeek mock.

Every real-client case performs the file-read tool cycle and verifies model
identity, authentication failure and upstream credential isolation. The final
case uses a distinct client token at each gateway hop. These are deterministic
mock tests; no live paid DeepSeek call is claimed.

The subprocess dry-run regression was RED on the original binary, which
rejected `--harness opencode`, and GREEN on the feature binary. The focused
Rust OpenCode tests and `bash lint --static-only` also passed. The broader
Rust library regression suite and remaining lint/hosted checks are tracked
separately from this acceptance evidence.

## Model limits and compaction

OpenCode custom providers do not inherit Models.dev limits. The route therefore
supplies a positive context window, preferring clud's canonical served model
metadata. The adapter fills missing native-default metadata from reviewed
[Claude](https://platform.claude.com/docs/en/models/overview),
[DeepSeek](https://api-docs.deepseek.com/quick_start/pricing/), and
[Kimi K3](https://platform.kimi.ai/docs/guide/kimi-k3-quickstart) specifications
(2026-11-02): current Claude API IDs use 1,000,000 tokens; the reviewed
DeepSeek and Kimi defaults use 1,048,576. The adapter strips the context suffix
only for this metadata lookup; API model ID conventions remain unchanged.

The default output request budget is 32,000 tokens, below these native models'
published maxima. This is a request budget, not a statement of their maximum
output capacity. Positive explicit inline `provider.clud.models.<wire>.limit`
settings remain authoritative. Uncataloged models without reviewed metadata
must supply both positive `context` and `output` limits, with output smaller
than context. Zero/invalid limits fail before spawn instead of silently
turning off OpenCode's automatic compaction. The acceptance suite includes a
fresh-config real-client case that supplies no pre-populated model limits.


After the compaction review fix, the combined recording-harness, real-client,
and served-gateway regression run passed **23 tests in 93.61 seconds**.
`bash lint --host-only` passed formatting, Clippy, repository guards and host
Dylint. The one-primary-reviewer pre-push review is clean after fixing the
model-limit metadata and architecture-table findings. A full serial Rust
library regression run uses an isolated `CLUD_DATA_DB`; its result is recorded
with PR validation rather than inferred from these integration checks.


The full Rust library regression suite passed serially with an isolated GC
database (539.53 seconds including compilation/lock waits):

```sh
CLUD_DATA_DB=/path/to/isolated/data.redb \
SOLDR_PORCELAIN=1 soldr cargo test -p clud --lib -- --test-threads=1
```

This avoids contending with the developer daemon's database. No daemon tests
were skipped or weakened.

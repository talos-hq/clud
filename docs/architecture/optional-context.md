# Optional external context

Enable the optional Hermes personality reference with:

```sh
clud config set context.hermes.enabled=true
```

Disable it with `clud config set context.hermes.enabled=false`. The default
is disabled. `config set` accepts dotted `KEY=VALUE` assignments and uses the
existing native desktop approval flow for settings changes. The preview and
deletion-root protections are the same as `clud settings set`; `--dry-run`
validates an assignment and reports the proposed change without requesting
approval or saving it. `clud config show --json` reports the saved setting.
The repo's `.clud/settings.json` can override the global value.

For Claude and Codex launches, `optional_context::prepare` resolves this setting
once. When enabled, `setup_command` exposes a narrow reference-only entry point
to its private Hermes boundary. It resolves `SOUL.md` in the current instance's
`HERMES_HOME`, or `~/.hermes` by default. An empty environment value selects the
default. The path is canonicalized, verified to be a file, and quoted in the
reference. A missing file produces an actionable launch error. Disabled launches
do not inspect the Hermes environment or filesystem.

The generated instruction identifies the file as optional personality context
and says that the agent may read it when useful. Clud reads no persona contents,
Hermes configuration, memories, or credentials for this feature. In particular,
it does not import a selected personality from Hermes' configuration; the
reference points specifically to `SOUL.md`.

The launch builder merges the reference with existing instructions: Claude uses
`--append-system-prompt`, and Codex uses `developer_instructions`. Built-in system
instructions, user-supplied instructions, and Clud's existing rules remain in
place. Neither `--system-prompt` nor `model_instructions_file` is used. This also
applies when model routing selects a provider behind the Claude or Codex harness.
Interactive, print, and loop launches share the same builder. Existing sessions
retain their prepared launch plan; start a new session to adopt a changed setting.

## Research and naming

`context.hermes.enabled` names the specific optional-context integration rather
than suggesting that Hermes is the running harness or that its entire prompt
replaces the native prompt.

- [Hermes personality](https://hermes-agent.nousresearch.com/docs/user-guide/features/personality/)
  defines `SOUL.md` as the instance's persistent identity file.
- [Codex configuration](https://learn.chatgpt.com/docs/config-file/config-reference)
  documents additive `developer_instructions` and the replacing
  `model_instructions_file` setting.
- [Claude CLI](https://code.claude.com/docs/en/cli-reference) documents appending
  to the system prompt independently of replacing it.

Unit tests cover settings precedence, validation, reference-only resolution,
and additive instruction merging. `tests/test_optional_context.py` checks both
harnesses' actual dry-run plans and the command's validation path.

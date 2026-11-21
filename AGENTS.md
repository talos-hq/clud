# AGENTS.md

Agent guidance for this repository lives in [`CLAUDE.md`](CLAUDE.md).

Do not add new GitHub Actions workflow files (`.github/workflows/*.yml` or
`*.yaml`) unless the user specifically requests them.

This file exists for tools that look for `AGENTS.md` by default (Codex, some agent harnesses). The actual conventions — build / lint / test commands, repository map, architecture pointers, design decisions, and the cross-cutting registries you must update when extending the codebase — are all there.

CLI syntax: [`docs/cli-grammar.md`](docs/cli-grammar.md) (generated). Agent index: [`llms.txt`](llms.txt).

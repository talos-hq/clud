# Documentation

Architecture and design records for clud. Read whichever doc matches what you're
working on; nothing here needs to be read in order.

- **[AFFIRMATIONS_OF_COVENANTS.md](AFFIRMATIONS_OF_COVENANTS.md)** - covenantal affirmations for the spirit of the project.
- **[ARCHITECTURE.md](ARCHITECTURE.md)** - index of subsystem topic docs (loop, daemon IPC, session lifecycle, skill system, gc/registry, Windows quirks, launch plan).
- **[cli-grammar.md](cli-grammar.md)** - generated reference for every clud command, flag, passthrough rule, multicall name and exit code (never hand-edit; [DD-234](DESIGN_DECISIONS.md#dd-234-the-cli-grammar-is-generated-from-clap-and-checked-for-drift-not-hand-written)).
- **[DESIGN_DECISIONS.md](DESIGN_DECISIONS.md)** - ADR-style records for non-obvious design choices.
- **[architecture/](architecture/)** - the subsystem docs themselves.

Per-directory `README.md` files under `crates/` and `testbins/` describe
**what's in this directory**. The docs here describe **how subsystems work
across directories**.

See the root [`CLAUDE.md`](../CLAUDE.md) for build / lint / test commands and
the rule for where new docs go.

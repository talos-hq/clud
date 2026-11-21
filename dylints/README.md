# dylints/

Custom Dylint lints for clud. Dylint crates are excluded from the stable
workspace build because they use rustc internals and pin their own nightly.

- `ban_manual_slash_normalize` bans hand-rolled `.replace('\\', "/")` path
  separator rewrites and directs callers to `clud::path_norm`.
- `ban_dirs_home_dir` bans `dirs::home_dir`, `std::env::home_dir` and their
  kin outside `crates/clud-bin/src/home.rs`, directing callers to
  `clud::home::user_home` (#1836). `ci/banned_home_dir.py` enforces the same
  rule in `bash lint` on every PR, because this lane runs off the PR path.
- `enforce_platform_boundary` denies naming a host outside a platform
  boundary, the same rule soldr, zccache and fbuild enforce (#1946, #1953).
  It covers host keys (`windows`, `unix`, `target_os`, ...) in
  `#[cfg]`/`#[cfg_attr]`/`cfg!`/`cfg_if!`/`cfg_select!`, and paths into
  `libc`, `windows_sys`, `windows`, `winapi`, `nix` and `mach2`.
  - **Where a host may be named:** only in a `platform/mod.rs` selector and
    the `windows/`/`unix/`/`linux/`/`macos/` trees under it, whether that is
    a crate's `src/platform/` or a `platform/` directory in a test suite.
  - **No exceptions:** there is no baseline and no allowlist, and tests are
    not exempt.
  - **Pre-expansion:** inactive host branches count, so the Linux run checks
    Windows and macOS code too.
  - **Tests:** `ui/boundary.rs` pins the rule against real rustc
    diagnostics. See
    [`docs/architecture/platform-boundary.md`](../docs/architecture/platform-boundary.md).

Run locally on the host (lint never runs under Bosn):

```bash
soldr lint                  # fmt, Clippy, then every library here
soldr lint --cross-dylint   # plus a Dylint pass per declared cross target
```

`[workspace.metadata.soldr.lint] steps` in the root `Cargo.toml` includes
`dylint`, so `soldr lint` (and `bash lint`, which calls it) runs these
libraries; local-gate.toml's `dylint` lane runs `soldr lint rust --check
--cross-dylint`. Hosted CI (`_dylint.yml`) still calls `soldr dylint` directly.

A late lint only sees code compiled for the checked target, so CI also runs a
check-only pass per OS from Linux (`--target x86_64-pc-windows-msvc` and
`--target aarch64-apple-darwin`). `soldr dylint prepare --target <triple>`
installs that target's nightly `rust-std` first.

Soldr's verified 6.2.0 Dylint lane supplies precompiled `cargo-dylint`,
`dylint-link`, and the matching nightly driver for each supported host. Keep `dylint_linting` at
**6.2.0** with those tools, and keep the nightly above matched to
`ban_manual_slash_normalize/rust-toolchain.toml`. `tests/test_dylint_stack.py`
asserts that lockstep.

Each library pins its nightly in `rust-toolchain.toml`, the file Soldr's
Dylint preparer reads, so `soldr lint` selects the matching verified tool,
linker and driver set with no extra environment. Hosted CI also sets
`SOLDR_DYLINT_TOOLCHAIN=nightly-2026-08-15` and clears an inherited
`RUSTUP_TOOLCHAIN` for `soldr dylint prepare`, so CI's stable selector cannot
override the pin. (The pre-6.0.4 bare `rust-toolchain` filename is gone:
Dylint 6.2.0 reads `rust-toolchain.toml`.)

`.cargo/config.toml` in the lint crate is load-bearing, not boilerplate. It
routes linking through `dylint-link`, the wrapper that names the cdylib
`lib<name>@<toolchain>.so` -- the exact filename Dylint looks up after building.
Without it the build succeeds and emits a plain `lib<name>.so`, and Dylint
fails with "Could not find ... despite successful build".

That was the real cause of the failure CI used to work around by copying the
artifact to the suffixed name and re-running Dylint. It read as an upstream
artifact-naming gap; it was a missing linker config here, absent because this
crate was not created from Dylint's template. The linker config is required for the blessed 6.2.0 path.

If the missing-alias failure ever recurs, check `.cargo/config.toml` before
suspecting Dylint.

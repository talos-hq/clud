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

Run locally:

```bash
export SOLDR_DYLINT_TOOLCHAIN=nightly-2026-08-15
env -u RUSTUP_TOOLCHAIN soldr dylint prepare
env -u RUSTUP_TOOLCHAIN soldr dylint --all -- --workspace --all-targets
```

A late lint only sees code compiled for the checked target, so CI also runs a
check-only pass per OS from Linux (`--target x86_64-pc-windows-msvc` and
`--target aarch64-apple-darwin`). `soldr dylint prepare --target <triple>`
installs that target's nightly `rust-std` first.

Soldr's verified 6.2.0 Dylint lane supplies precompiled `cargo-dylint`,
`dylint-link`, and the matching nightly driver for each supported host. Keep `dylint_linting` at
**6.2.0** with those tools, and keep the nightly above matched to
`ban_manual_slash_normalize/rust-toolchain`. `tests/test_dylint_stack.py`
asserts that lockstep.

Clearing an inherited `RUSTUP_TOOLCHAIN` for `soldr dylint prepare` is
intentional: CI's stable Rust selector must not override the lint crate's
nightly pin. Set `SOLDR_DYLINT_TOOLCHAIN=nightly-2026-08-15` for these commands:
Soldr's preparer reads `rust-toolchain.toml`, while this crate intentionally
retains Dylint's bare `rust-toolchain` file. Soldr then selects the matching
verified tool, linker, and driver set.

The legacy bare `rust-toolchain` filename is intentional. Dylint 6.0.3 unsets
`RUSTUP_TOOLCHAIN` while building its driver and recognizes this filename when
selecting the lint crate's nightly. Renaming it to `rust-toolchain.toml` makes
6.0.3 fall back to stable and fail; upgrading to 6.0.4 only to recognize that
newer filename would also give up Soldr's blessed precompiled fast path.

`.cargo/config.toml` in the lint crate is load-bearing, not boilerplate. It
routes linking through `dylint-link`, the wrapper that names the cdylib
`lib<name>@<toolchain>.so` -- the exact filename Dylint looks up after building.
Without it the build succeeds and emits a plain `lib<name>.so`, and Dylint
fails with "Could not find ... despite successful build".

That was the real cause of the failure CI used to work around by copying the
artifact to the suffixed name and re-running Dylint. It read as an upstream
artifact-naming gap; it was a missing linker config here, absent because this
crate was not created from Dylint's template. The linker config and the legacy
toolchain filename are both required for the blessed 6.2.0 path.

If the missing-alias failure ever recurs, check `.cargo/config.toml` before
suspecting Dylint.

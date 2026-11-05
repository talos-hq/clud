# platform/

The host-platform boundary. This is the only place in `clud-bin`'s sources
where a host may be named. Rules and rationale:
[`docs/architecture/platform-boundary.md`](../../../../docs/architecture/platform-boundary.md).

- `stdio` (#2386): `redirect_stderr_to(&File)` points the process stderr at a file (`dup2` / `SetStdHandle`); used by the daemon entry.
- `session_tmp_live`: verifies protected desktop-service identity and manager lineage for temp GC; unsupported hosts retain the unknown-process safeguard.
- `mod.rs` is the selector. It declares `unix` or `windows` and re-exports
  the selected tree, so `crate::platform::<area>` resolves to the current
  host's implementation.
- `windows/` and `unix/` hold one module per area with the same public
  items in both trees: for example `host` (the `WINDOWS`/`UNIX`/`LINUX`/`MACOS`
  consts) and `subprocess` (the suspended spawn and the `.cmd`/`.bat`
  rewrite). Each tree's `mod.rs` lists its areas.
- When a concept exists on one host only, the other tree's module is a stub
  that preserves that host's behaviour.

To add an area, create `windows/<area>.rs` and `unix/<area>.rs` with matching
signatures, declare `pub mod <area>;` in both `mod.rs` files, and call
`crate::platform::<area>::..` from the neutral module.

## Kernel fixtures

`unix/process_identity_zombie_tests.rs` contains the Linux-only `waitid(WNOWAIT)`
regression for zombie liveness (#1993). `unix/mod.rs` includes it as
a test module so the native API and host selection stay inside this boundary.
The raw child remains unreaped during direct and batched identity observations,
then is reaped before the assertions. No production wait API is introduced.

`unix/credential_file.rs` and `windows/credential_file.rs` validate the opened
credential file before reading: ownership, regular-file identity, and private
permissions or a protected owner-only DACL. `mod.rs` selects the host tree.

`unix/secret_prompt.rs` checks the Linux keyboard runtime before entering
winit, avoiding a loader panic on minimal desktops. It follows winit's
Wayland-first selection; the other hosts retain typed graphics initialization.
Windows selects the Direct3D 12 renderer with the system FXC compiler; Unix
selects OpenGL.

`start_terminal` supplies the generated Bash adapter and hidden launch helper
for explicit Windows Command Prompt windows; Unix supplies an empty adapter.
The persistence contract is documented in
[process-reaping.md](../../../../docs/architecture/process-reaping.md#explicit-command-prompt-windows-on-windows-2007).

`stdio::output_is_utf8` supplies native console capability to [foreground print output](../../../../docs/architecture/prompt-output.md).

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
selects OpenGL. These graphics adapters remain for settings windows.
Credential acquisition delegates exclusively to lokr; see
[secret-prompt.md](../../../../docs/architecture/secret-prompt.md).

`start_terminal` supplies the generated Bash adapter and hidden launch helper
for explicit Windows Command Prompt windows; Unix supplies an empty adapter.
The persistence contract is documented in
[process-reaping.md](../../../../docs/architecture/process-reaping.md#explicit-command-prompt-windows-on-windows-2007).

`stdio::output_is_utf8` supplies native console capability to [foreground print output](../../../../docs/architecture/prompt-output.md).

`app_dialog` is `Install clud.app`'s native surface (#2863): `osascript`
dialogs with the text passed as argv and the Terminal hand-off through
`open -a Terminal`. Windows has a stub; the bundle exists only on macOS.

`remote_shell` is `clud-remote-shell`, the remote-assistance PTY's session
leader that shares a sudo ticket with the operator's shell, plus the client's
private socket to it; `console_echo` hides the sudo password while it is typed.
Both are Unix implementations with Windows stubs (sudo is not offered on
Windows). See [Request sudo](../../../../docs/architecture/remote-assistance.md#request-sudo)
and [DD-216](../../../../docs/DESIGN_DECISIONS.md#dd-216-remote-sudo-is-validated-by-a-clud-session-leader-inside-the-operator-pty).

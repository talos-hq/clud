"""Fold the notarized macOS binaries into one universal clud, wrap it in a
stapled ``Install clud.app`` and a stapled disk image, and prove both pass
Gatekeeper offline (#2863).

A bare Mach-O cannot carry a notarization ticket (``stapler`` takes only
``.app``, ``.dmg`` and ``.pkg``), so a browser-downloaded bare ``clud`` passes
Gatekeeper only when the Mac can reach Apple. The disk image fixes that:

``build --dist-dir dist --out-dir macos-installer --version X`` runs in Auto
Release's ``package-macos`` job on a macOS runner, after ``notarize-macos``:

1. ``lipo`` the two signed, notarized thin ``clud`` executables from the macOS
   wheels into ``clud-X-universal-apple-darwin`` and prove the slices are
   byte-identical to the wheel executables, then sign the universal file as
   one (identifier ``clud``, hardened runtime). Per-slice signatures from
   ``lipo`` run standalone but ``codesign --verify`` rejects them as nested
   code in the app ("nested code is modified"). The re-signed file is the
   published asset and the app's helper, so the app's notarization covers it.
2. Assemble ``Install clud.app``: ``Contents/MacOS/clud`` is the universal
   binary, which recognizes the Finder launch and hands off to
   ``Contents/Helpers/clud`` (see ``self_install/app_bundle.rs``). The helper
   is the universal binary unchanged: signing the app binds the main
   executable's signature to the bundle, so only the helper may be copied
   out and installed. Sign the app with the Developer ID Application certificate from a temporary
   keychain, notarize it and staple the ticket.
3. Put the stapled app and ``READ ME if blocked.txt`` on ``clud-X-macos.dmg``
   (``hdiutil``), sign, notarize and staple the image.

``gate --dir macos-installer --version X`` runs on both macOS architectures
with Apple's ticket service blocked (``/etc/hosts``) and the files
quarantined as a browser would. A negative control proves the block works:
the bare universal binary, which carries no ticket, must *not* assess as
notarized. The stapled image and app must, the helper must be the exact
universal binary, and the helper must install
with ``--installer --install-current --yes`` and resolve by name in a fresh
login shell.
"""

from __future__ import annotations

import argparse
import contextlib
import json
import os
import plistlib
import secrets
import shutil
import socket
import sys
import tempfile
import time
from pathlib import Path

from ci import process
from ci.sign_macos_release import (
    NOTARIZED_SOURCE,
    NotaryConfig,
    SigningConfig,
    SigningError,
    _private_file,
    config_from_env,
    ditto_command,
    macos_wheels,
    notary_config_from_env,
    submit_for_notarization,
    wheel_machos,
)

APP_NAME = "Install clud.app"
BUNDLE_ID = "com.talos-hq.clud.installer"
# The x86_64 wheel's floor (macosx_10_15); arm64 needs 11.0 anyway.
MINIMUM_MACOS = "10.15"
UNIVERSAL_ARCHS = ("x86_64", "arm64")
# Gatekeeper's online notarization-ticket lookup.
TICKET_HOSTS = ("api.apple-cloudkit.com",)
HOSTS_MARKER = "# clud offline Gatekeeper gate"
PLAN_SCHEMA = "clud-installer-plan/v1"
README_NAME = "READ ME if blocked.txt"
README = """\
Install clud
============

Double-click "Install clud". It shows what it will install and where, and
changes nothing until you click Install.

If macOS says the app cannot be opened:
  1. Open System Settings > Privacy & Security.
  2. Scroll down and click "Open Anyway" next to the message about
     "Install clud", then confirm.

Prefer a terminal? Either of these installs the same release:
  curl -fsSL https://talos-hq.github.io/clud/install.sh | sh
  "/Volumes/<this disk>/Install clud.app/Contents/Helpers/clud" --installer

https://talos-hq.github.io/clud/
"""


def universal_name(version: str) -> str:
    return f"clud-{version}-universal-apple-darwin"


def dmg_name(version: str) -> str:
    return f"clud-{version}-macos.dmg"


def info_plist(version: str) -> bytes:
    """The bundle's Info.plist: the universal clud is the main executable."""
    return plistlib.dumps(
        {
            "CFBundleDevelopmentRegion": "en",
            "CFBundleDisplayName": "Install clud",
            "CFBundleExecutable": "clud",
            "CFBundleIdentifier": BUNDLE_ID,
            "CFBundleInfoDictionaryVersion": "6.0",
            "CFBundleName": "Install clud",
            "CFBundlePackageType": "APPL",
            "CFBundleShortVersionString": version,
            "CFBundleVersion": version,
            "LSMinimumSystemVersion": MINIMUM_MACOS,
            # No Dock icon: the installer talks only through dialogs.
            "LSUIElement": True,
            "NSHighResolutionCapable": True,
        }
    )


def _quiet(command: list[str], **kwargs) -> tuple[int, str]:
    """Run without echoing ``command`` (argv may hold a password)."""
    completed = process.run(command, capture_output=True, text=True, check=False, **kwargs)
    return completed.returncode, f"{completed.stdout or ''}{completed.stderr or ''}"


def _quiet_notary(command: list[str]) -> tuple[int, str, str]:
    completed = process.run(command, capture_output=True, text=True, check=False)
    return completed.returncode, completed.stdout or "", completed.stderr or ""


def _require(command: list[str], what: str, **kwargs) -> str:
    code, output = _quiet(command, **kwargs)
    if code != 0:
        raise SigningError(f"{what} failed (exit {code}): {output.strip()}")
    return output


def thin_executables(dist_dir: Path) -> dict[str, bytes]:
    """The signed ``clud`` of each macOS wheel, keyed by its Mach-O arch."""
    found: dict[str, bytes] = {}
    for wheel in macos_wheels(dist_dir):
        clud = [
            data
            for name, data in wheel_machos(wheel).items()
            if name.endswith(".data/scripts/clud")
        ]
        if len(clud) != 1:
            raise SigningError(f"{wheel.name} must hold exactly one clud executable")
        arch = "arm64" if "arm64" in wheel.name else "x86_64"
        if arch in found:
            raise SigningError(f"two macOS wheels for {arch}")
        found[arch] = clud[0]
    if set(found) != set(UNIVERSAL_ARCHS):
        raise SigningError(f"need one x86_64 and one arm64 wheel, found {sorted(found)}")
    return found


def lipo_archs(path: Path) -> set[str]:
    return set(_require(["lipo", "-archs", str(path)], "lipo -archs").split())


def make_universal(thin: dict[str, bytes], output: Path, scratch: Path) -> None:
    """``lipo`` the slices and prove each is byte-identical to its input."""
    inputs = []
    for arch, data in sorted(thin.items()):
        path = scratch / f"clud-{arch}"
        path.write_bytes(data)
        inputs.append(str(path))
    _require(["lipo", "-create", "-output", str(output), *inputs], "lipo -create")
    output.chmod(0o755)
    if lipo_archs(output) != set(UNIVERSAL_ARCHS):
        raise SigningError(f"{output.name} lacks {UNIVERSAL_ARCHS}")
    for arch, data in thin.items():
        extracted = scratch / f"extracted-{arch}"
        _require(["lipo", str(output), "-thin", arch, "-output", str(extracted)], "lipo -thin")
        if extracted.read_bytes() != data:
            raise SigningError(f"{arch} slice differs from the notarized wheel executable")
    print(f"universal: {output.name} = {' + '.join(sorted(thin))}, slices byte-identical")


@contextlib.contextmanager
def signing_keychain(config: SigningConfig, scratch: Path):
    """A throwaway keychain holding only the Developer ID certificate."""
    password = secrets.token_hex(24)
    keychain = scratch / "clud-release.keychain-db"
    p12 = _private_file(scratch / "developer-id.p12", config.cert_p12)
    code, listed = _quiet(["security", "list-keychains", "-d", "user"])
    previous = [line.strip().strip('"') for line in listed.splitlines() if line.strip()]
    _require(["security", "create-keychain", "-p", password, str(keychain)], "create-keychain")
    try:
        _require(["security", "set-keychain-settings", "-lut", "3600", str(keychain)], "settings")
        _require(["security", "unlock-keychain", "-p", password, str(keychain)], "unlock")
        _require(
            [
                "security", "import", str(p12), "-k", str(keychain), "-f", "pkcs12",
                "-P", config.cert_password, "-T", "/usr/bin/codesign",
            ],
            "import the Developer ID certificate",
        )
        _require(
            [
                "security", "set-key-partition-list", "-S", "apple-tool:,apple:,codesign:",
                "-s", "-k", password, str(keychain),
            ],
            "set-key-partition-list",
        )
        _require(
            ["security", "list-keychains", "-d", "user", "-s", str(keychain), *previous],
            "list-keychains",
        )
        yield keychain
    finally:
        if code == 0:
            _quiet(["security", "list-keychains", "-d", "user", "-s", *previous])
        _quiet(["security", "delete-keychain", str(keychain)])
        p12.unlink(missing_ok=True)


def codesign(target: Path, identity: str, keychain: Path, *, runtime: bool,
             identifier: str | None = None) -> None:
    command = ["codesign", "--force", "--timestamp", "--keychain", str(keychain)]
    if runtime:
        command += ["--options", "runtime"]
    if identifier:
        command += ["--identifier", identifier]
    _require([*command, "--sign", identity, str(target)], f"codesign {target.name}")
    _require(["codesign", "--verify", "--strict", "--deep", "--verbose=2", str(target)],
             f"codesign --verify {target.name}")


def staple(target: Path) -> None:
    _require(["xcrun", "stapler", "staple", str(target)], f"stapler staple {target.name}")
    _require(["xcrun", "stapler", "validate", str(target)], f"stapler validate {target.name}")
    print(f"stapled: {target.name}")


def assemble_app(universal: Path, destination: Path, version: str) -> Path:
    app = destination / APP_NAME
    (app / "Contents" / "MacOS").mkdir(parents=True)
    (app / "Contents" / "Helpers").mkdir()
    (app / "Contents" / "Info.plist").write_bytes(info_plist(version))
    shutil.copy2(universal, app / "Contents" / "MacOS" / "clud")
    # Signed already (each slice); codesign without --deep leaves it as is.
    shutil.copy2(universal, app / "Contents" / "Helpers" / "clud")
    return app


def notarize_app(app: Path, notary: NotaryConfig, scratch: Path) -> None:
    upload = scratch / "install-clud-app.zip"
    _require(ditto_command(app, upload), "ditto")
    submit_for_notarization(upload, notary, APP_NAME, _quiet_notary)
    staple(app)


def build_dmg(app: Path, output: Path, version: str, scratch: Path) -> None:
    stage = scratch / "dmg-root"
    stage.mkdir()
    shutil.copytree(app, stage / APP_NAME, symlinks=True)
    (stage / README_NAME).write_text(README, encoding="utf-8")
    _require(
        [
            "hdiutil", "create", "-volname", f"Install clud {version}", "-srcfolder", str(stage),
            "-fs", "HFS+", "-format", "UDZO", "-ov", "-o", str(output),
        ],
        "hdiutil create",
    )


def assess(target: Path, *args: str) -> str:
    code, verdict = _quiet(["spctl", "--assess", *args, "-vv", str(target)], timeout=300)
    print(f"spctl {target.name}: {verdict.strip()}")
    return verdict if code == 0 else ""


def build(dist_dir: Path, out_dir: Path, version: str, env: dict[str, str]) -> None:
    signing = config_from_env(env)
    notary = notary_config_from_env(env)
    out_dir.mkdir(parents=True, exist_ok=True)
    universal = out_dir / universal_name(version)
    dmg = out_dir / dmg_name(version)
    with tempfile.TemporaryDirectory(prefix="clud-macpkg-") as scratch_dir:
        scratch = Path(scratch_dir)
        make_universal(thin_executables(dist_dir), universal, scratch)
        with signing_keychain(signing, scratch) as keychain:
            # The lipo'd per-slice signatures run standalone but fail as
            # nested code in the app ("nested code is modified"), so sign the
            # universal file as one; the app's notarization covers it.
            codesign(universal, signing.identity, keychain, runtime=True, identifier="clud")
            if lipo_archs(universal) != set(UNIVERSAL_ARCHS):
                raise SigningError(f"signing {universal.name} dropped a slice")
            app = assemble_app(universal, scratch, version)
            codesign(app, signing.identity, keychain, runtime=True)
            notarize_app(app, notary, scratch)
            build_dmg(app, dmg, version, scratch)
            codesign(dmg, signing.identity, keychain, runtime=False)
        submit_for_notarization(dmg, notary, dmg.name, _quiet_notary)
        staple(dmg)
    if NOTARIZED_SOURCE not in assess(dmg, "--type", "open", "--context",
                                      "context:primary-signature"):
        raise SigningError(f"{dmg.name} does not assess as {NOTARIZED_SOURCE!r}")
    for path in (universal, dmg):
        print(f"built: {path.name} ({path.stat().st_size} bytes)")


def quarantine(path: Path) -> None:
    """Mark ``path`` as a browser download (what Safari writes)."""
    value = f"0083;{int(time.time()):x};Safari;"
    _require(["xattr", "-w", "com.apple.quarantine", value, str(path)], "xattr -w")


def hosts_lines() -> str:
    return "".join(
        f"{address} {host} {HOSTS_MARKER}\n"
        for host in TICKET_HOSTS
        for address in ("127.0.0.1", "::1")
    )


TICKET_DAEMONS = ("syspolicyd", "cloudd")


@contextlib.contextmanager
def ticket_service_blocked():
    """Make Gatekeeper's ticket lookup unreachable, and prove it is."""
    _require(["sudo", "tee", "-a", "/etc/hosts"], "block hosts", input=hosts_lines())
    _quiet(["sudo", "dscacheutil", "-flushcache"])
    _quiet(["sudo", "killall", "-HUP", "mDNSResponder"])
    # The ticket lookup goes through CloudKit daemons whose connections,
    # opened before the block, never re-resolve; launchd restarts them on
    # demand, so the next lookup must resolve through /etc/hosts. Without
    # this the negative control saw a notarized verdict on an arm64 runner.
    for daemon in TICKET_DAEMONS:
        _quiet(["sudo", "killall", "-9", daemon])
    try:
        for host in TICKET_HOSTS:
            addresses = {info[4][0] for info in socket.getaddrinfo(host, 443)}
            if not addresses <= {"127.0.0.1", "::1"}:
                raise SigningError(f"{host} still resolves to {addresses}; not offline")
        yield
    finally:
        _quiet(["sudo", "sed", "-i", "", f"/{HOSTS_MARKER}/d", "/etc/hosts"])
        _quiet(["sudo", "dscacheutil", "-flushcache"])


def installer_env(home: Path) -> dict[str, str]:
    return {
        "HOME": str(home),
        "SHELL": "/bin/zsh",
        "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
        "TMPDIR": os.environ.get("TMPDIR", "/tmp"),
        "LANG": "en_US.UTF-8",
    }


RESULT_SCHEMA = "clud-installer-result/v1"


def installer_result(output: str, schema: str = RESULT_SCHEMA) -> dict:
    """The ``--installer-json`` (or plan) object with ``schema``. The captured
    output interleaves stderr with stdout, and the JSON has no trailing
    newline, so decode at each ``{`` rather than per line."""
    decoder = json.JSONDecoder()
    start = output.rfind("{")
    while start != -1:
        try:
            value, _ = decoder.raw_decode(output, start)
        except json.JSONDecodeError:
            value = None
        if isinstance(value, dict) and value.get("schema") == schema:
            return value
        start = output.rfind("{", 0, start)
    raise SigningError(f"no {schema} line in the installer output:\n{output}")


def install_from_app(clud: Path, version: str, home: Path) -> None:
    """The app's clud plans read-only, installs, and resolves by name."""
    env = installer_env(home)
    base = [str(clud), "--installer", "--install-current"]
    plan = installer_result(
        _require([*base, "--installer-plan-json"], "plan", env=env, timeout=120), PLAN_SCHEMA
    )
    if plan.get("version") != version:
        raise SigningError(f"unexpected installer plan: {plan}")
    if Path(plan["destination"]).exists():
        raise SigningError("--installer-plan-json wrote the destination")
    output = _require([*base, "--yes", "--installer-json"], "install", env=env, timeout=300)
    result = installer_result(output)
    if result.get("status") != "complete" or result.get("version") != version:
        raise SigningError(f"app install was not complete: {result}")
    lookup = _require(
        ["/bin/zsh", "-l", "-c", "command -v clud && clud --version"],
        "fresh login shell lookup", env=env, timeout=60,
    )
    if f"clud {version}" not in lookup:
        raise SigningError(f"fresh login shell did not resolve clud {version}: {lookup}")
    print(f"installed from {APP_NAME} and resolved by name: {lookup.split()[-1]}")


def gate(directory: Path, version: str) -> None:
    with tempfile.TemporaryDirectory(prefix="clud-macgate-") as scratch_dir:
        scratch = Path(scratch_dir)
        dmg = scratch / dmg_name(version)
        bare = scratch / universal_name(version)
        shutil.copy2(directory / dmg_name(version), dmg)
        shutil.copy2(directory / universal_name(version), bare)
        for path in (dmg, bare):
            quarantine(path)
        if lipo_archs(bare) != set(UNIVERSAL_ARCHS):
            raise SigningError(f"{bare.name} is not universal")
        with ticket_service_blocked():
            # Negative control (and the RED of #2863): a bare binary carries
            # no ticket, so offline it must not assess as notarized.
            if NOTARIZED_SOURCE in assess(bare, "--type", "install"):
                raise SigningError("offline simulation ineffective: the bare binary still "
                                   "assessed as notarized, so the ticket service was reached")
            if NOTARIZED_SOURCE not in assess(dmg, "--type", "open", "--context",
                                              "context:primary-signature"):
                raise SigningError(f"stapled {dmg.name} is not notarized offline")
            mount = scratch / "mnt"
            mount.mkdir()
            _require(["hdiutil", "attach", "-nobrowse", "-readonly", "-noautoopen",
                      "-mountpoint", str(mount), str(dmg)], "hdiutil attach")
            try:
                gate_mounted(mount, version, bare)
            finally:
                _quiet(["hdiutil", "detach", "-force", str(mount)])
    print(f"offline Gatekeeper gate passed for {dmg_name(version)}")


def gate_mounted(mount: Path, version: str, universal: Path) -> None:
    app = mount / APP_NAME
    if not (mount / README_NAME).is_file():
        raise SigningError(f"the disk image lacks {README_NAME!r}")
    if NOTARIZED_SOURCE not in assess(app, "--type", "execute"):
        raise SigningError(f"stapled {APP_NAME} is not notarized offline")
    _require(["codesign", "--verify", "--strict", "--deep", "--verbose=2", str(app)],
             "codesign --verify app")
    clud = app / "Contents" / "Helpers" / "clud"
    if clud.read_bytes() != universal.read_bytes():
        raise SigningError(f"{APP_NAME}'s helper is not the published universal binary")
    if lipo_archs(app / "Contents" / "MacOS" / "clud") != set(UNIVERSAL_ARCHS):
        raise SigningError(f"{APP_NAME}'s main executable is not universal")
    # The installer refuses a HOME reached through a symlinked ancestor, and
    # macOS's temporary directory is under /var -> /private/var. A scratch
    # home under the runner's real home is what a user's home looks like.
    home = Path(tempfile.mkdtemp(prefix="clud-gate-home-", dir=Path.home().resolve()))
    try:
        install_from_app(clud, version, home)
    finally:
        shutil.rmtree(home, ignore_errors=True)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=(__doc__ or "").splitlines()[0])
    sub = parser.add_subparsers(dest="mode", required=True)
    build_parser = sub.add_parser("build")
    build_parser.add_argument("--dist-dir", type=Path, required=True)
    build_parser.add_argument("--out-dir", type=Path, required=True)
    build_parser.add_argument("--version", required=True)
    gate_parser = sub.add_parser("gate")
    gate_parser.add_argument("--dir", type=Path, required=True)
    gate_parser.add_argument("--version", required=True)
    args = parser.parse_args(argv)
    try:
        if args.mode == "build":
            build(args.dist_dir, args.out_dir, args.version, dict(os.environ))
        else:
            gate(args.dir, args.version)
    except SigningError as exc:
        print(f"ERROR: {exc}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())

"""Download page: browser OS/arch detection, the asset dropdown, and install.sh."""

from __future__ import annotations

import json
import os
import shutil
import stat
from html.parser import HTMLParser
from pathlib import Path

import pytest

from installer import site
from installer.site import render_page
from tests.process import run as run_process

REPO = Path(__file__).resolve().parents[1]
DETECT_JS = REPO / "installer" / "download.js"
INSTALL_SH = REPO / "installer" / "install.sh"

VERSION = "2.8.56"
TRIPLES = {
    ("linux", "x86_64"): "x86_64-unknown-linux-musl",
    ("linux", "aarch64"): "aarch64-unknown-linux-musl",
    ("darwin", "x86_64"): "x86_64-apple-darwin",
    ("darwin", "aarch64"): "aarch64-apple-darwin",
    ("windows", "x86_64"): "x86_64-pc-windows-msvc.exe",
    ("windows", "aarch64"): "aarch64-pc-windows-msvc.exe",
}

MAC_SAFARI = (
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 "
    "(KHTML, like Gecko) Version/17.4 Safari/605.1.15"
)
CHROME_WIN = (
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
    "(KHTML, like Gecko) Chrome/130.0.0.0 Safari/537.36"
)

# (case id, navigator facts, expected os, expected arch, recommended?)
DETECTION_TABLE = [
    (
        "macos-apple-silicon-client-hints",
        {
            "uaPlatform": "macOS",
            "uaArch": "arm",
            "uaBitness": "64",
            "platform": "MacIntel",
            "userAgent": MAC_SAFARI,
        },
        "darwin",
        "aarch64",
        True,
    ),
    (
        "macos-intel-client-hints",
        {
            "uaPlatform": "macOS",
            "uaArch": "x86",
            "uaBitness": "64",
            "platform": "MacIntel",
            "userAgent": MAC_SAFARI,
        },
        "darwin",
        "x86_64",
        True,
    ),
    (
        "macos-apple-silicon-safari-webgl",
        {"platform": "MacIntel", "userAgent": MAC_SAFARI, "webglRenderer": "Apple GPU"},
        "darwin",
        "aarch64",
        True,
    ),
    (
        "macos-intel-safari-webgl",
        {
            "platform": "MacIntel",
            "userAgent": MAC_SAFARI,
            "webglRenderer": "Intel(R) Iris(TM) Plus Graphics 655",
        },
        "darwin",
        "x86_64",
        True,
    ),
    (
        # The frozen "Intel Mac OS X" UA must not be read as an Intel CPU.
        "macos-arch-unknown",
        {"platform": "MacIntel", "userAgent": MAC_SAFARI},
        "darwin",
        None,
        False,
    ),
    (
        "linux-x64-platform",
        {
            "platform": "Linux x86_64",
            "userAgent": "Mozilla/5.0 (X11; Linux x86_64; rv:131.0) Gecko/20100101 Firefox/131.0",
        },
        "linux",
        "x86_64",
        True,
    ),
    (
        "linux-arm64-platform",
        {
            "platform": "Linux aarch64",
            "userAgent": "Mozilla/5.0 (X11; Linux aarch64; rv:131.0) Gecko/20100101 Firefox/131.0",
        },
        "linux",
        "aarch64",
        True,
    ),
    (
        "linux-arm64-client-hints",
        {
            "uaPlatform": "Linux",
            "uaArch": "arm",
            "uaBitness": "64",
            "platform": "Linux x86_64",
            "userAgent": "Mozilla/5.0 (X11; Linux x86_64)",
        },
        "linux",
        "aarch64",
        True,
    ),
    (
        "windows-x64-user-agent",
        {"platform": "Win32", "userAgent": CHROME_WIN},
        "windows",
        "x86_64",
        True,
    ),
    (
        "windows-arm64-client-hints",
        {
            "uaPlatform": "Windows",
            "uaArch": "arm",
            "uaBitness": "64",
            "platform": "Win32",
            "userAgent": CHROME_WIN,
        },
        "windows",
        "aarch64",
        True,
    ),
    (
        "windows-arm64-user-agent",
        {
            "platform": "Win32",
            "userAgent": "Mozilla/5.0 (Windows NT 10.0; ARM64) AppleWebKit/537.36",
        },
        "windows",
        "aarch64",
        True,
    ),
    (
        "android-phone-has-no-binary",
        {
            "platform": "Linux armv8l",
            "userAgent": "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 Mobile",
        },
        None,
        None,
        False,
    ),
    (
        "iphone-has-no-binary",
        {
            "platform": "iPhone",
            "userAgent": "Mozilla/5.0 (iPhone; CPU iPhone OS 17_4 like Mac OS X) Mobile/15E148",
        },
        None,
        None,
        False,
    ),
    (
        "ipad-desktop-mode-has-no-binary",
        {"platform": "MacIntel", "userAgent": MAC_SAFARI, "maxTouchPoints": 5},
        None,
        None,
        False,
    ),
    (
        "junk-agent",
        {"platform": "", "userAgent": "curl/8.9.1"},
        None,
        None,
        False,
    ),
    (
        "unknown-agent",
        {"platform": "PlayStation 5", "userAgent": "Mozilla/5.0 (PlayStation; PlayStation 5/2.26)"},
        None,
        None,
        False,
    ),
    (
        "linux-32bit-no-guess",
        {"platform": "Linux i686", "userAgent": "Mozilla/5.0 (X11; Linux i686)"},
        "linux",
        None,
        False,
    ),
]


def assets_catalog() -> dict:
    platforms = []
    for (os_name, arch), triple in TRIPLES.items():
        filename = f"clud-{VERSION}-{triple}"
        item = {
            "platform": {"os": os_name, "arch": arch},
            "asset": {
                "filename": filename,
                "media_type": "application/octet-stream",
                "urls": [
                    f"https://github.com/talos-hq/clud/releases/download/{VERSION}/{filename}"
                ],
            },
        }
        if os_name == "linux":
            item["variant"] = {"flavor": "static-musl"}
        platforms.append(item)
    return {
        "channels": {"latest-stable": VERSION},
        "releases": [{"version": VERSION, "platforms": platforms}],
    }


def run_detection(cases: list[dict]) -> list[dict]:
    node = shutil.which("node")
    assert node, "node is required to test the download page's detection script"
    program = (
        "const d = require(process.argv[1]);"
        "const cases = JSON.parse(process.argv[2]);"
        "console.log(JSON.stringify(cases.map((c) => d.detect(c))));"
    )
    done = run_process(
        [node, "-e", program, str(DETECT_JS), json.dumps(cases)],
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert done.returncode == 0, done.stderr
    return json.loads(done.stdout)


def test_detection_table_maps_every_agent_to_the_right_asset_or_none() -> None:
    results = run_detection([facts for _, facts, *_ in DETECTION_TABLE])
    rows = [
        {"os": os_name, "arch": arch, "filename": f"clud-{VERSION}-{triple}"}
        for (os_name, arch), triple in TRIPLES.items()
    ]
    node = shutil.which("node")
    for (case, _facts, os_name, arch, recommended), got in zip(
        DETECTION_TABLE, results, strict=True
    ):
        assert (got["os"], got["arch"], got["recommended"]) == (os_name, arch, recommended), case
        if recommended:
            assert got["label"], case
    # The recommendation resolves to exactly one published asset name.
    program = (
        "const d = require(process.argv[1]);"
        "const r = JSON.parse(process.argv[2]); const rows = JSON.parse(process.argv[3]);"
        "const names = r.map((x) => { const a = d.assetFor(x, rows); return a && a.filename; });"
        "console.log(JSON.stringify(names.map((n) => n || null)));"
    )
    done = run_process(
        [node, "-e", program, str(DETECT_JS), json.dumps(results), json.dumps(rows)],
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert done.returncode == 0, done.stderr
    names = json.loads(done.stdout)
    for (case, _facts, os_name, arch, recommended), name in zip(
        DETECTION_TABLE, names, strict=True
    ):
        expected = f"clud-{VERSION}-{TRIPLES[(os_name, arch)]}" if recommended else None
        assert name == expected, case


class Page(HTMLParser):
    def __init__(self) -> None:
        super().__init__()
        self.options: list[dict[str, str]] = []
        self.ids: set[str] = set()
        self.scripts: list[str] = []
        self._in_script = False

    def handle_starttag(self, tag, attrs) -> None:
        values = dict(attrs)
        if values.get("id"):
            self.ids.add(values["id"])
        if tag == "option" and values.get("value"):
            self.options.append(values)
        self._in_script = tag == "script"

    def handle_data(self, data) -> None:
        if self._in_script:
            self.scripts.append(data)

    def handle_endtag(self, tag) -> None:
        if tag == "script":
            self._in_script = False


def test_page_dropdown_references_every_published_asset_of_the_release() -> None:
    page = render_page(assets_catalog())
    parsed = Page()
    parsed.feed(page)
    published = {f"clud-{VERSION}-{triple}" for triple in TRIPLES.values()}
    in_dropdown = {option["value"].rsplit("/", 1)[-1] for option in parsed.options}
    assert in_dropdown == published
    for name in published:
        assert name in page
    assert {
        "platform-select",
        "recommended",
        "detected",
        "one-liner",
        "copy-one-liner",
    } <= parsed.ids
    assert "curl -fsSL https://talosbot.ai/clud/install.sh | sh" in page
    assert '<meta name="viewport"' in page
    assert DETECT_JS.read_text(encoding="utf-8").strip() in "".join(parsed.scripts)


def test_site_serves_download_page_at_root_and_install_sh(tmp_path) -> None:
    catalog = assets_catalog()
    page = render_page(catalog)
    assert site.STATIC_ASSETS["install.sh"] == INSTALL_SH.read_bytes()
    assert "install.sh" in site.published_paths()
    assert site.root_page(catalog) == page


# ---------------------------------------------------------------- install.sh

# install.sh is the macOS/Linux route; its stubs are POSIX shell scripts.
posix_only = pytest.mark.skipif(os.name == "nt", reason="install.sh targets macOS and Linux")


def make_stubs(tmp_path: Path, uname_s: str, uname_m: str, sums: str, digest: str) -> dict:
    stubs = tmp_path / "stubs"
    stubs.mkdir()
    log = tmp_path / "curl.log"

    def script(name: str, body: str) -> None:
        path = stubs / name
        path.write_text("#!/bin/sh\n" + body, encoding="utf-8")
        path.chmod(path.stat().st_mode | stat.S_IXUSR | stat.S_IXGRP | stat.S_IXOTH)

    script(
        "uname",
        f'case "$1" in -s) echo {uname_s};; -m) echo {uname_m};; *) echo {uname_s};; esac\n',
    )
    script("sysctl", "exit 1\n")
    script(
        "curl",
        f'echo "$*" >> "{log}"\n'
        'out=""; url=""; head=0\n'
        'while [ $# -gt 0 ]; do case "$1" in\n'
        '  -o) out="$2"; shift;; -I) head=1;; -w) shift;; -*) ;; *) url="$1";;\n'
        "esac; shift; done\n"
        'if [ "$head" = 1 ]; then\n'
        '  printf "https://github.com/talos-hq/clud/releases/tag/9.9.9"; exit 0\n'
        "fi\n"
        'case "$url" in\n'
        '  */SHA256SUMS) printf "%s" "$STUB_SUMS" > "$out";;\n'
        '  *) printf "binary" > "$out";;\n'
        "esac\n",
    )
    script("sha256sum", f'printf "%s  %s\\n" "{digest}" "$1"\n')
    script("shasum", f'printf "%s  %s\\n" "{digest}" "$3"\n')
    env = {
        "PATH": f"{stubs}{os.pathsep}{os.environ['PATH']}",
        "HOME": str(tmp_path / "home"),
        "STUB_SUMS": sums,
    }
    return {"env": env, "log": log}


def run_install(
    tmp_path: Path, args: list[str], uname_s="Linux", uname_m="x86_64", sums="", digest="0" * 64
):
    stubs = make_stubs(tmp_path, uname_s, uname_m, sums, digest)
    sh = shutil.which("sh")
    assert sh
    done = run_process(
        [sh, str(INSTALL_SH), *args], capture_output=True, text=True, env=stubs["env"], timeout=60
    )
    calls = stubs["log"].read_text(encoding="utf-8").splitlines() if stubs["log"].exists() else []
    return done, calls


@posix_only
@pytest.mark.parametrize(
    ("uname_s", "uname_m", "triple"),
    [
        ("Linux", "x86_64", "x86_64-unknown-linux-musl"),
        ("Linux", "amd64", "x86_64-unknown-linux-musl"),
        ("Linux", "aarch64", "aarch64-unknown-linux-musl"),
        ("Linux", "arm64", "aarch64-unknown-linux-musl"),
        ("Darwin", "x86_64", "x86_64-apple-darwin"),
        ("Darwin", "arm64", "aarch64-apple-darwin"),
    ],
)
def test_install_sh_dry_run_names_the_asset_and_fetches_nothing(
    tmp_path, uname_s, uname_m, triple
) -> None:
    done, calls = run_install(tmp_path, ["--dry-run", "--version", "2.8.56"], uname_s, uname_m)
    assert done.returncode == 0, done.stderr
    out = done.stdout + done.stderr
    asset = f"clud-2.8.56-{triple}"
    assert f"https://github.com/talos-hq/clud/releases/download/2.8.56/{asset}" in out
    assert "https://github.com/talos-hq/clud/releases/download/2.8.56/SHA256SUMS" in out
    assert str(tmp_path / "home" / ".local" / "bin" / "clud") in out
    assert calls == []
    assert not (tmp_path / "home").exists()


@posix_only
def test_install_sh_dry_run_resolves_latest_with_a_head_request_only(tmp_path) -> None:
    done, calls = run_install(tmp_path, ["--dry-run"])
    assert done.returncode == 0, done.stderr
    assert "clud-9.9.9-x86_64-unknown-linux-musl" in done.stdout + done.stderr
    assert len(calls) == 1
    assert "-I" in calls[0].split()
    assert "-o /dev/null" in calls[0]


@posix_only
def test_install_sh_installs_verified_binary(tmp_path) -> None:
    digest = "a" * 64
    sums = f"{digest}  ./clud-2.8.56-x86_64-unknown-linux-musl\n{'b' * 64}  ./other\n"
    done, _ = run_install(tmp_path, ["--version", "2.8.56"], sums=sums, digest=digest)
    assert done.returncode == 0, done.stderr
    installed = tmp_path / "home" / ".local" / "bin" / "clud"
    assert installed.read_text(encoding="utf-8") == "binary"
    assert os.access(installed, os.X_OK)
    assert "clud setup" in done.stdout + done.stderr
    assert not (tmp_path / "home" / ".profile").exists()


@posix_only
def test_install_sh_refuses_checksum_mismatch(tmp_path) -> None:
    sums = f"{'a' * 64}  ./clud-2.8.56-x86_64-unknown-linux-musl\n"
    done, _ = run_install(tmp_path, ["--version", "2.8.56"], sums=sums, digest="c" * 64)
    assert done.returncode != 0
    assert "checksum mismatch" in done.stderr
    assert not (tmp_path / "home" / ".local" / "bin" / "clud").exists()


@posix_only
def test_install_sh_refuses_asset_missing_from_sums(tmp_path) -> None:
    done, _ = run_install(tmp_path, ["--version", "2.8.56"], sums=f"{'a' * 64}  ./other\n")
    assert done.returncode != 0
    assert "SHA256SUMS" in done.stderr


@posix_only
@pytest.mark.parametrize(
    ("uname_s", "uname_m"), [("Linux", "i686"), ("FreeBSD", "amd64"), ("Linux", "riscv64")]
)
def test_install_sh_rejects_unsupported_platform_naming_supported_ones(
    tmp_path, uname_s, uname_m
) -> None:
    done, calls = run_install(tmp_path, ["--dry-run", "--version", "2.8.56"], uname_s, uname_m)
    assert done.returncode != 0
    for supported in ("Linux x86_64", "Linux aarch64", "macOS x86_64", "macOS aarch64"):
        assert supported in done.stderr
    assert calls == []


@posix_only
def test_install_sh_write_profile_is_opt_in(tmp_path) -> None:
    digest = "a" * 64
    sums = f"{digest}  ./clud-2.8.56-x86_64-unknown-linux-musl\n"
    done, _ = run_install(
        tmp_path, ["--version", "2.8.56", "--write-profile"], sums=sums, digest=digest
    )
    assert done.returncode == 0, done.stderr
    profile = (tmp_path / "home" / ".profile").read_text(encoding="utf-8")
    assert ".local/bin" in profile


@posix_only
def test_install_sh_rejects_unknown_flag(tmp_path) -> None:
    done, _ = run_install(tmp_path, ["--bogus"])
    assert done.returncode != 0
    assert "--bogus" in done.stderr

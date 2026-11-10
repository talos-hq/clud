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
MAC_FIREFOX = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10.15; rv:131.0) Gecko/20100101 Firefox/131.0"
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
        # Safari and Firefox expose no client hints: a labelled Apple Silicon guess.
        "macos-safari-no-client-hints-assumes-apple-silicon",
        {"platform": "MacIntel", "userAgent": MAC_SAFARI},
        "darwin",
        "aarch64",
        True,
    ),
    (
        "macos-firefox-no-client-hints-assumes-apple-silicon",
        {"platform": "MacIntel", "userAgent": MAC_FIREFOX},
        "darwin",
        "aarch64",
        True,
    ),
    (
        # A 32-bit hint is a real answer with no binary: never overridden by a guess.
        "macos-32bit-client-hint-no-guess",
        {"uaPlatform": "macOS", "uaArch": "x86", "uaBitness": "32", "platform": "MacIntel"},
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


MAC_ASSUMPTION_STATUS = "Detected: macOS · assuming Apple Silicon (aarch64) — most Macs since 2020."


def run_node(program: str, *args: str) -> object:
    node = shutil.which("node")
    assert node, "node is required to test the download page's detection script"
    done = run_process(
        [node, "-e", program, str(DETECT_JS), *args], capture_output=True, text=True, timeout=60
    )
    assert done.returncode == 0, done.stderr
    return json.loads(done.stdout)


def test_detection_marks_only_hintless_macs_as_a_labelled_assumption() -> None:
    cases = {case: facts for case, facts, *_ in DETECTION_TABLE}
    names = list(cases)
    results = dict(zip(names, run_detection([cases[n] for n in names]), strict=True))
    assumed = {name for name, got in results.items() if got["assumed"]}
    assert assumed == {
        "macos-safari-no-client-hints-assumes-apple-silicon",
        "macos-firefox-no-client-hints-assumes-apple-silicon",
    }
    for name in assumed:
        got = results[name]
        assert "assuming" in got["label"], name
        assert "Apple Silicon" in got["label"], name
        assert got["override"] == {"label": "Switch to Intel.", "os": "darwin", "arch": "x86_64"}
    # Client hints are a real answer: no assumption, no override.
    for name in ("macos-apple-silicon-client-hints", "macos-intel-client-hints"):
        assert results[name]["assumed"] is False, name
        assert results[name]["override"] is None, name
        assert "assuming" not in results[name]["label"], name
    for name in ("junk-agent", "unknown-agent", "iphone-has-no-binary"):
        assert results[name]["override"] is None, name
        assert results[name]["recommended"] is False, name


FAKE_DOM = r"""
const d = require(process.argv[1]);
const rows = JSON.parse(process.argv[2]);
const facts = JSON.parse(process.argv[3]);
function el(id) {
  return { id, hidden: false, textContent: "", href: null, value: "", listeners: {},
    attrs: {}, addEventListener(t, f) { this.listeners[t] = f; },
    removeAttribute(n) { if (n === "href") this.href = null; },
    setAttribute(n, v) { this.attrs[n] = v; } };
}
const els = {};
for (const id of ["platform-select", "recommended", "recommended-name", "detected-text",
  "arch-override", "copy-one-liner", "one-liner"]) els[id] = el(id);
els["platform-select"].options = rows.map((r) => ({ value: r.url, textContent: r.filename,
  dataset: { os: r.os, arch: r.arch, filename: r.filename,
    downloadUrl: r.downloadUrl, downloadName: r.downloadName } }));
const doc = { getElementById: (id) => els[id] };
const nav = { platform: facts.platform, userAgent: facts.userAgent, maxTouchPoints: 0 };
if (facts.uaArch) {
  nav.userAgentData = { platform: facts.uaPlatform,
    getHighEntropyValues: () => Promise.resolve({ architecture: facts.uaArch, bitness: "64" }) };
}
const snap = () => ({ href: els.recommended.hidden ? null : els.recommended.href,
  status: els["detected-text"].textContent, override: els["arch-override"].hidden ? null
    : els["arch-override"].textContent });
d.start(nav, doc).then(() => {
  const before = snap();
  if (!els["arch-override"].hidden) els["arch-override"].listeners.click();
  console.log(JSON.stringify({ before, after: snap() }));
});
"""


def page_rows() -> list[dict[str, str]]:
    base = f"https://github.com/talos-hq/clud/releases/download/{VERSION}"
    return [
        {"os": o, "arch": a, "filename": f"clud-{VERSION}-{t}", "url": f"{base}/clud-{VERSION}-{t}"}
        for (o, a), t in TRIPLES.items()
    ]


def asset_url(os_name: str, arch: str) -> str:
    triple = TRIPLES[(os_name, arch)]
    return f"https://github.com/talos-hq/clud/releases/download/{VERSION}/clud-{VERSION}-{triple}"


def test_switch_to_intel_override_changes_the_recommended_href() -> None:
    facts = {"platform": "MacIntel", "userAgent": MAC_SAFARI}
    got = run_node(FAKE_DOM, json.dumps(page_rows()), json.dumps(facts))
    assert got["before"]["href"] == asset_url("darwin", "aarch64")
    assert got["before"]["status"] == MAC_ASSUMPTION_STATUS
    assert got["before"]["override"] == "Switch to Intel."
    assert got["after"]["href"] == asset_url("darwin", "x86_64")
    assert got["after"]["override"] is None
    assert "Intel (x86_64)" in got["after"]["status"]


@pytest.mark.parametrize(
    ("facts", "expected"),
    [
        ({"uaPlatform": "macOS", "uaArch": "arm", "platform": "MacIntel"}, ("darwin", "aarch64")),
        ({"uaPlatform": "macOS", "uaArch": "x86", "platform": "MacIntel"}, ("darwin", "x86_64")),
    ],
)
def test_user_agent_data_answer_is_used_without_an_override(facts, expected) -> None:
    got = run_node(FAKE_DOM, json.dumps(page_rows()), json.dumps(facts))
    assert got["before"]["href"] == asset_url(*expected)
    assert got["before"]["override"] is None
    assert "assuming" not in got["before"]["status"]


def test_unknown_agent_gets_no_recommendation_and_no_override() -> None:
    facts = {"platform": "", "userAgent": "curl/8.9.1"}
    got = run_node(FAKE_DOM, json.dumps(page_rows()), json.dumps(facts))
    assert got["before"] == {
        "href": None,
        "status": "Could not detect a supported system. Choose your download below.",
        "override": None,
    }


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
        "detected-text",
        "arch-override",
        "recommended-name",
    } <= parsed.ids
    assert "curl -fsSL https://talos-hq.github.io/clud/install.sh | sh" in page
    assert '<meta name="viewport"' in page
    assert DETECT_JS.read_text(encoding="utf-8").strip() in "".join(parsed.scripts)


class Assets(HTMLParser):
    """Every attribute that makes a browser fetch something to render the page."""

    def __init__(self) -> None:
        super().__init__()
        self.fetched: list[str] = []

    def handle_starttag(self, tag, attrs) -> None:
        values = dict(attrs)
        for name in ("src", "srcset", "poster", "data"):
            if values.get(name):
                self.fetched.append(values[name])
        if tag == "link" and values.get("href"):
            self.fetched.append(values["href"])


def test_page_is_self_contained_with_no_external_assets() -> None:
    page = render_page(assets_catalog())
    parsed = Assets()
    parsed.feed(page)
    assert parsed.fetched == []
    lowered = page.lower()
    for needle in ("@import", "url(http", "url(//", "@font-face", "fonts.googleapis"):
        assert needle not in lowered, needle
    # Styled, responsive and accessible, without a framework or build step.
    for needle in ("prefers-reduced-motion", ":focus-visible", "@media", "color-scheme:dark"):
        assert needle in page, needle


def test_site_serves_download_page_at_root_and_install_sh(tmp_path) -> None:
    catalog = assets_catalog()
    page = render_page(catalog)
    assert site.STATIC_ASSETS["install.sh"] == INSTALL_SH.read_bytes()
    assert "install.sh" in site.published_paths()
    assert site.root_page(catalog) == page


# ---------------------------------------------------------------- install.sh

# install.sh is the macOS/Linux route; its stubs are POSIX shell scripts.
posix_only = pytest.mark.skipif(os.name == "nt", reason="install.sh targets macOS and Linux")


def installer_stub(tmp_path: Path) -> str:
    """Shell that stands in for `clud --installer --install-current`.

    It records its argv and whether stdin was the terminal file, copies itself
    to $CLUD_INSTALL_DIR/clud like the real installer, and prints the outcome
    JSON. STUB_OUTCOME picks declined / failed / activation_failed paths.
    """
    log = tmp_path / "installer.log"
    return (
        'if [ "$1" = --installer ]; then\n'
        f'  echo "argv:$*" >> "{log}"\n'
        f'  echo "dir:$CLUD_INSTALL_DIR" >> "{log}"\n'
        '  [ -n "${CLUD_INSTALL_TTY:-}" ] && [ "$(readlink /proc/$$/fd/0 2>/dev/null)" = '
        f'"$CLUD_INSTALL_TTY" ] && echo "stdin:tty" >> "{log}"\n'
        '  case "${STUB_OUTCOME:-complete}" in\n'
        '    declined) echo \'{"status":"refused","binary_committed":false}\'; exit 0;;\n'
        '    failed) echo \'{"status":"failed","binary_committed":false}\'; exit 1;;\n'
        "  esac\n"
        '  mkdir -p "$CLUD_INSTALL_DIR" && cp "$0" "$CLUD_INSTALL_DIR/clud"\n'
        '  echo \'{"status":"complete","binary_committed":true}\'\n'
        '  [ "${STUB_OUTCOME:-}" = activation_failed ] && exit 1\n'
        "  exit 0\n"
        "fi\n"
    )


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
        '  *) printf "%s" "${STUB_BINARY:-binary}" > "$out";;\n'
        "esac\n",
    )
    script("sha256sum", f'printf "%s  %s\\n" "{digest}" "$1"\n')
    script("shasum", f'printf "%s  %s\\n" "{digest}" "$3"\n')
    env = {
        "PATH": f"{stubs}{os.pathsep}{os.environ['PATH']}",
        "HOME": str(tmp_path / "home"),
        "STUB_SUMS": sums,
        "STUB_BINARY": "#!/bin/sh\n" + installer_stub(tmp_path) + "exit 0\n",
        # Never reach the developer's real terminal; tests opt in with a file.
        "CLUD_INSTALL_TTY": "",
        "SHELL": "/bin/sh",
    }
    return {"env": env, "log": log}


def run_install(
    tmp_path: Path,
    args: list[str],
    uname_s="Linux",
    uname_m="x86_64",
    sums="",
    digest="0" * 64,
    env: dict | None = None,
):
    stubs = make_stubs(tmp_path, uname_s, uname_m, sums, digest)
    stubs["env"].update(env or {})
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
def test_install_sh_v_prefixed_tag_names_the_unprefixed_asset(tmp_path) -> None:
    """#2786: a v-prefixed tag keeps its v in the URL path, never in the asset name."""
    done, calls = run_install(tmp_path, ["--dry-run", "--version", "v2.8.65"])
    assert done.returncode == 0, done.stderr
    out = done.stdout + done.stderr
    assert (
        "https://github.com/talos-hq/clud/releases/download/v2.8.65/"
        "clud-2.8.65-x86_64-unknown-linux-musl"
    ) in out
    assert "clud-v2.8.65" not in out
    assert calls == []


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
    assert "--installer" in installed.read_text(encoding="utf-8")
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


def handoff(tmp_path, args=(), tty=None, env=None):
    """Run install.sh and return (result, installer argv, installer log lines)."""
    extra = dict(env or {})
    if tty is not None:
        tty_file = tmp_path / "tty"
        tty_file.write_text(tty, encoding="utf-8")
        extra["CLUD_INSTALL_TTY"] = str(tty_file)
    done, _ = run_install(
        tmp_path, ["--version", "2.8.56", *args], sums=SUMS, digest=DIGEST, env=extra
    )
    log = tmp_path / "installer.log"
    lines = log.read_text(encoding="utf-8").splitlines() if log.exists() else []
    argv = next((line[5:].split() for line in lines if line.startswith("argv:")), None)
    return done, argv, lines


BASE = ["--installer", "--install-current", "--installer-json"]


@posix_only
def test_install_sh_hands_activation_to_the_binary_on_a_terminal(tmp_path) -> None:
    # ~/.local/bin is not on PATH (the macOS default): Rust plans and asks.
    done, argv, lines = handoff(tmp_path, tty="")
    assert done.returncode == 0, done.stderr
    assert argv == BASE
    assert f"dir:{tmp_path / 'home' / '.local' / 'bin'}" in lines
    if Path("/proc/self/fd/0").exists():
        assert "stdin:tty" in lines
    assert (tmp_path / "home" / ".local" / "bin" / "clud").exists()
    # install.sh itself never edits a startup file any more.
    home = tmp_path / "home"
    for rc in (".zprofile", ".zshrc", ".bash_profile", ".bashrc", ".profile"):
        assert not (home / rc).exists()


@posix_only
def test_install_sh_without_a_terminal_installs_without_activation(tmp_path) -> None:
    done, argv, _ = handoff(tmp_path)
    assert done.returncode == 0, done.stderr
    assert argv == [*BASE, "--yes", "--no-modify-path"]


@posix_only
def test_install_sh_skips_activation_when_the_directory_is_on_path(tmp_path) -> None:
    bin_dir = tmp_path / "home" / ".local" / "bin"
    path = f"{bin_dir}{os.pathsep}{os.environ['PATH']}"
    stubs = tmp_path / "stubs"
    done, argv, _ = handoff(tmp_path, tty="", env={"PATH": f"{stubs}{os.pathsep}{path}"})
    assert done.returncode == 0, done.stderr
    assert argv == [*BASE, "--no-modify-path"]


@posix_only
def test_install_sh_write_profile_activates_without_asking(tmp_path) -> None:
    done, argv, _ = handoff(tmp_path, args=["--write-profile"], tty="")
    assert done.returncode == 0, done.stderr
    assert argv == [*BASE, "--yes"]


@posix_only
def test_install_sh_no_modify_path_is_passed_through(tmp_path) -> None:
    done, argv, _ = handoff(tmp_path, args=["--no-modify-path"], tty="")
    assert done.returncode == 0, done.stderr
    assert argv == [*BASE, "--no-modify-path"]


@posix_only
def test_install_sh_honors_clud_install_dir(tmp_path) -> None:
    custom = tmp_path / "opt" / "bin"
    done, _, lines = handoff(tmp_path, env={"CLUD_INSTALL_DIR": str(custom)})
    assert done.returncode == 0, done.stderr
    assert f"dir:{custom}" in lines
    assert (custom / "clud").exists()


@posix_only
def test_install_sh_declined_install_changes_nothing(tmp_path) -> None:
    done, _, _ = handoff(tmp_path, tty="", env={"STUB_OUTCOME": "declined"})
    assert done.returncode == 0, done.stderr
    assert "clud was not installed." in done.stdout
    assert "Next step" not in done.stdout
    assert not (tmp_path / "home" / ".local" / "bin" / "clud").exists()


@posix_only
def test_install_sh_failed_install_exits_non_zero(tmp_path) -> None:
    done, _, _ = handoff(tmp_path, tty="", env={"STUB_OUTCOME": "failed"})
    assert done.returncode != 0
    assert "did not finish" in done.stderr


@posix_only
def test_install_sh_activation_failure_keeps_the_installed_binary(tmp_path) -> None:
    done, _, _ = handoff(tmp_path, tty="", env={"STUB_OUTCOME": "activation_failed"})
    assert done.returncode == 0, done.stderr
    assert "shell activation did not finish" in done.stdout
    assert (tmp_path / "home" / ".local" / "bin" / "clud").exists()


@posix_only
def test_install_sh_rejects_unknown_flag(tmp_path) -> None:
    done, _ = run_install(tmp_path, ["--bogus"])
    assert done.returncode != 0
    assert "--bogus" in done.stderr


# ------------------------------------------------- setup offer after install

ASSET = "clud-2.8.56-x86_64-unknown-linux-musl"
DIGEST = "c" * 64
SUMS = f"{DIGEST}  ./{ASSET}\n"


def fake_clud(tmp_path: Path, check_exit: int, setup_exit: int = 0) -> tuple[str, Path]:
    """A stand-in clud binary: logs its argv and stdin, answers --check."""
    log = tmp_path / "clud-calls.log"
    body = (
        "#!/bin/sh\n" + installer_stub(tmp_path) + f'echo "argv:$*" >> "{log}"\n'
        f'if [ "$*" = "setup --check" ]; then exit {check_exit}; fi\n'
        f'if [ "$1" = setup ]; then IFS= read -r line; echo "stdin:$line" >> "{log}"; '
        f"exit {setup_exit}; fi\n"
        "exit 0\n"
    )
    return body, log


def install_with(tmp_path: Path, answer: str | None, check_exit: int, args=(), **kw):
    body, log = fake_clud(tmp_path, check_exit, **kw)
    env = {"STUB_BINARY": body}
    if answer is not None:
        tty = tmp_path / "tty"
        tty.write_text(answer, encoding="utf-8")
        env["CLUD_INSTALL_TTY"] = str(tty)
    done, _ = run_install(
        tmp_path, ["--version", "2.8.56", *args], sums=SUMS, digest=DIGEST, env=env
    )
    calls = log.read_text(encoding="utf-8").splitlines() if log.exists() else []
    return done, calls


@posix_only
def test_install_sh_offers_setup_and_runs_the_installed_binary_on_yes(tmp_path) -> None:
    done, calls = install_with(tmp_path, "y\nopenrouter\n", check_exit=3)
    assert done.returncode == 0, done.stderr
    assert "Set up clud now? [y/n]" in done.stdout
    assert calls[0] == "argv:setup --check"
    assert calls[1] == "argv:setup"
    # setup reads the terminal, not the piped script.
    assert calls[2] == "stdin:openrouter"


@posix_only
@pytest.mark.parametrize("answer", ["n\n", "\n", "no\n", ""])
def test_install_sh_skips_setup_unless_the_user_says_yes(tmp_path, answer) -> None:
    done, calls = install_with(tmp_path, answer, check_exit=3)
    assert done.returncode == 0, done.stderr
    assert "Set up clud now? [y/n]" in done.stdout
    assert calls == ["argv:setup --check"]
    assert "Next step: run 'clud setup'" in done.stdout


@posix_only
def test_install_sh_does_not_offer_setup_when_already_set_up(tmp_path) -> None:
    done, calls = install_with(tmp_path, "y\n", check_exit=0)
    assert done.returncode == 0, done.stderr
    assert "Set up clud now?" not in done.stdout
    assert calls == ["argv:setup --check"]


@posix_only
@pytest.mark.parametrize("check_exit", [1, 2, 127])
def test_install_sh_does_not_offer_setup_when_the_check_is_unsupported(
    tmp_path, check_exit
) -> None:
    # An older binary without --check exits 2 (usage); only 3 means "needs setup".
    done, calls = install_with(tmp_path, "y\n", check_exit=check_exit)
    assert done.returncode == 0, done.stderr
    assert "Set up clud now?" not in done.stdout
    assert "argv:setup" not in calls


@posix_only
def test_install_sh_never_prompts_without_a_terminal(tmp_path) -> None:
    done, calls = install_with(tmp_path, None, check_exit=3)
    assert done.returncode == 0, done.stderr
    assert "Set up clud now?" not in done.stdout
    assert calls == []
    assert "Next step: run 'clud setup'" in done.stdout


@posix_only
def test_install_sh_no_setup_flag_skips_the_offer(tmp_path) -> None:
    done, calls = install_with(tmp_path, "y\n", check_exit=3, args=("--no-setup",))
    assert done.returncode == 0, done.stderr
    assert calls == []


@posix_only
def test_install_sh_reports_a_failed_setup_with_the_full_path(tmp_path) -> None:
    done, _ = install_with(tmp_path, "y\n", check_exit=3, setup_exit=1)
    assert done.returncode == 1
    dest = tmp_path / "home" / ".local" / "bin" / "clud"
    assert f"run it again with: {dest} setup" in done.stderr


@posix_only
@pytest.mark.parametrize(
    ("env", "green"),
    [
        ({"CLUD_INSTALL_COLOR": "always"}, True),
        ({}, False),
        ({"CLUD_INSTALL_COLOR": "always", "NO_COLOR": "1"}, False),
    ],
)
def test_install_sh_prints_the_next_step_in_green_on_a_colour_terminal(
    tmp_path, env, green
) -> None:
    done, _ = run_install(tmp_path, ["--version", "2.8.56"], sums=SUMS, digest=DIGEST, env=env)
    assert done.returncode == 0, done.stderr
    coloured = "\033[32mNext step: run 'clud setup'\033[0m" in done.stdout
    assert coloured is green
    assert "Next step: run 'clud setup'" in done.stdout


def test_download_names_and_same_origin_bytes() -> None:
    parsed = Page()
    parsed.feed(render_page(assets_catalog()))
    for option in parsed.options:
        if not option.get("value"):
            continue
        name = "clud.exe" if option["data-os"] == "windows" else "clud"
        assert option["data-download-name"] == name
        assert option["data-download-url"].startswith("/clud/downloads/")
    page = render_page(assets_catalog())
    for name in ["clud", "clud.exe", "clud-setup", "clud-setup.exe"]:
        assert f'download="{name}"' in page


@pytest.mark.parametrize(
    ("facts", "os_name", "arch", "filename"),
    [
        ({"platform": "Win32", "userAgent": "Windows NT 10.0; Win64; x64"},
         "windows", "x86_64", "clud.exe"),
        ({"platform": "MacIntel", "userAgent": MAC_SAFARI}, "darwin", "aarch64", "clud"),
    ],
)
def test_recommendation_uses_same_origin_and_clean_filename(facts, os_name, arch, filename) -> None:
    rows = page_rows()
    for row in rows:
        row["downloadUrl"] = site.download_path(row)
        row["downloadName"] = "clud.exe" if row["os"] == "windows" else "clud"
    script = FAKE_DOM.replace("status: els[", "download: els.recommended.download, status: els[")
    got = run_node(script, json.dumps(rows), json.dumps(facts))
    row = next(row for row in rows if row["os"] == os_name and row["arch"] == arch)
    assert got["before"]["href"] == row["downloadUrl"]
    assert got["before"]["download"] == filename

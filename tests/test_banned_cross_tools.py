"""Fixtures for the #637 cross-compiler linter.

The linter's value is entirely in what it *rejects*, so the negative fixtures
are the point. The positive ones matter just as much in the other direction: a
rule that also rejects the Linux Zig path would be reverted within a day, and
Zig is the supported cross for `*-unknown-linux-gnu` and for the manylinux
wheel.

Fixtures are strings rather than files so a future command shape can be added
here in one line, and so the same table covers YAML, Python and shell — the
issue's "cannot bypass the rule by moving from YAML to Python or shell".
"""

from __future__ import annotations

import pytest

from ci import banned_cross_tools
from ci.banned_cross_tools import _scan_rust, _strip_rust_comments, main, scan_text

# --------------------------------------------------------------- rejected --

REJECTED: list[tuple[str, str, str]] = [
    (
        "cargo-xwin invocation, windows x86",
        "cargo xwin build --release --target x86_64-pc-windows-msvc",
        ".sh",
    ),
    (
        "cargo-xwin hyphenated form, windows arm",
        "cargo-xwin build --target aarch64-pc-windows-msvc",
        ".sh",
    ),
    (
        "zigbuild aimed at darwin arm",
        "cargo zigbuild --target aarch64-apple-darwin",
        ".sh",
    ),
    (
        "zigbuild aimed at darwin x86",
        "cargo-zigbuild build --target x86_64-apple-darwin",
        ".sh",
    ),
    (
        "maturin --zig aimed at darwin",
        "maturin build --zig --target aarch64-apple-darwin --release",
        ".sh",
    ),
    (
        "zig cc as the linker for an MSVC target",
        'env["CC_x86_64_pc_windows_msvc"] = "zig cc -target x86_64-pc-windows-msvc"',
        ".py",
    ),
    (
        "the `cross` wrapper at a darwin target",
        "cross build --target x86_64-apple-darwin",
        ".sh",
    ),
    # `build` is not the only verb that compiles for a foreign target.
    ("cross test", "cross test --target x86_64-pc-windows-msvc", ".sh"),
    ("cross run", "cross run --target x86_64-apple-darwin", ".sh"),
    ("cross check", "RUN cross check --target x86_64-pc-windows-msvc", ""),
    ("cross rustc", "cross rustc --target x86_64-pc-windows-msvc -- -C x", ".sh"),
    (
        "cross with a toolchain override",
        "cross +nightly build --target x86_64-pc-windows-msvc",
        ".sh",
    ),
    ("cross as an argv list", 'ARGS = ["cross", "build"]', ".py"),
    # Wrappers that take a command as their argument still put `cross` at a
    # command position, just not at the start of the line.
    ("sudo cross", "sudo cross build --target x86_64-pc-windows-msvc", ".sh"),
    ("env-prefixed cross", "env RUSTFLAGS=-C cross build", ".sh"),
    ("xargs cross", "xargs cross build", ".sh"),
    ("timeout-wrapped cross", "timeout 60 cross build", ".sh"),
    # Wrappers stack, and carry more than three arguments of their own. A
    # tighter bound was briefly used to kill a prose false positive that this
    # suite had invented and that existed nowhere in the tree — the trade goes
    # the other way: `time cross build` as prose and as an invocation are
    # textually identical, and the invocation is the one that costs CI time.
    # Prose that needs to say it can carry the allow marker.
    ("env with four assignments", "env A=1 B=2 C=3 D=4 cross build", ".sh"),
    ("stacked wrappers", "sudo -E env A=1 B=2 cross build", ".sh"),
    ("xargs with its own flags", "xargs -n1 -P4 -t cross build", ".sh"),
    ("time-wrapped cross", "time cross build --target aarch64-apple-darwin", ".sh"),
    ("nice-wrapped cross", "nice cross build --release", ".sh"),
    # The argv form must reach the same verbs as the shell form, with flags or
    # a toolchain override in between.
    ("cross argv with a toolchain", '["cross", "+nightly", "build"]', ".py"),
    ("cross argv with bench", '["cross", "bench"]', ".py"),
    ("cross argv with flags before the verb", '["cross", "--target", t, "build"]', ".py"),
    ("cargo install cross", "cargo install cross", ".sh"),
    # A string literal is not a comment: `let open = "/*"` must not blank out
    # every line until a later `"*/"`, or the tool is hidden by two strings.
    (
        "a command hidden between two block-comment string literals",
        'let open = "/*";\nlet cmd = "cargo xwin build";\nlet close = "*/";',
        ".rs",
    ),
    (
        "a // inside a string does not truncate the rest of the line",
        'let s = "a//b"; let c = "cargo xwin build";',
        ".rs",
    ),
    # A char literal holding a quote must not flip the scanner's phase. If it
    # does, the string on the next line is read as code, its `//` starts a
    # comment, and the URL naming the banned tool is blanked away — a missed
    # violation, and four files in this repo were already desynced this way.
    (
        "a cargo-xwin URL after a quote char literal",
        "let q = '\"';\nlet u = \"https://github.com/rust-cross/cargo-xwin\";",
        ".rs",
    ),
    (
        "a cargo-xwin string after an escaped-quote char literal",
        "let q = '\\'';\nlet u = \"cargo xwin build\";",
        ".rs",
    ),
    # A raw string has no escapes, so `r"\\?\"` ends at its own closing quote.
    # A scanner that honours the backslash reads past it and treats the rest
    # of the file as string — hiding every violation after it. Five such
    # literals are in this repo today.
    (
        "a cargo-xwin URL after a Windows raw-string path prefix",
        'fn f(p: &str) -> bool { p.starts_with(r"\\\\?\\") }\n'
        'let u = "https://github.com/rust-cross/cargo-xwin";',
        ".rs",
    ),
    (
        "osxcross for darwin",
        "osxcross-clang --target aarch64-apple-darwin",
        ".sh",
    ),
    # The rule must not be evadable by changing file type. Same command, three
    # syntaxes.
    (
        "same violation expressed in YAML",
        "        run: cargo xwin build --target x86_64-pc-windows-msvc",
        ".yml",
    ),
    (
        "same violation expressed in Python",
        'subprocess.run(["cargo", "xwin", "build", "--target", "x86_64-pc-windows-msvc"])',
        ".py",
    ),
    # Installs are banned at any target: a hand-installed wrapper is what makes
    # a hand-rolled invocation reachable.
    ("cargo install cargo-xwin", "cargo install cargo-xwin --locked", ".sh"),
    ("cargo install cargo-zigbuild", "cargo install cargo-zigbuild", ".sh"),
    ("pip install ziglang", "pip install ziglang==0.13.0", ".sh"),
    ("ziglang dependency declaration", '    "ziglang>=0.15.2,<0.16",', ".toml"),
    (
        "python module ziglang invocation",
        'subprocess.run([sys.executable, "-m", "ziglang", "c++"])',
        ".py",
    ),
    (
        "versioned Python module ziglang invocation",
        # Fixture string. python-name-lint: allow-next-line
        'subprocess.run(["python3.13", "-m", "ziglang", "c++"])',
        ".py",
    ),
    ("setup-zig action", "      - uses: goto-bus-stop/setup-zig@v2", ".yml"),
    ("mlugg setup-zig action", "      - uses: mlugg/setup-zig@v1", ".yml"),
    # ---------------------------------------------------------------- #714 --
    # Every one of these was verified to slip through the pre-#714 linter.
    # `xwin` is MSVC-only by construction, so requiring a literal triple on the
    # line made the two shapes a real workflow uses — a variable target, or a
    # target supplied elsewhere entirely — invisible.
    ("xwin at a variable target", "cargo xwin build --target $TARGET", ".sh"),
    ("xwin with no target on the line", "cargo xwin build --release", ".sh"),
    ("the bare xwin CLI splatting the CRT", "xwin splat --output /opt/xwin", ".sh"),
    # Flags sit between the binary and the subcommand in every real invocation,
    # including the one in xwin's own README. A rule that only matched the
    # adjacent form would be green here and blind in production.
    (
        "xwin with flags before the subcommand",
        "xwin --accept-license splat --output /opt/xwin",
        ".sh",
    ),
    (
        "xwin in a Dockerfile RUN with flags",
        "RUN xwin --arch x86_64 --sdk-version 10 splat --output /xwin",
        "",
    ),
    ("xwin as an argv list", '["xwin", "--accept-license", "splat"]', ".py"),
    ("XWIN_* environment", "XWIN_ACCEPT_LICENSE=1 cargo build", ".sh"),
    # `_` is a word character, so `\bXWIN_` never matches inside `CARGO_XWIN_`
    # — and `CARGO_XWIN_*` is cargo-xwin's own prefix, i.e. the half a real
    # user actually sets.
    ("CARGO_XWIN_* environment", "CARGO_XWIN_CROSS_COMPILER=clang-cl", ".sh"),
    (
        "CARGO_XWIN_* in a YAML env block",
        "      CARGO_XWIN_CROSS_COMPILER: clang-cl",
        ".yml",
    ),
    ("cargo binstall cargo-xwin", "cargo binstall cargo-xwin", ".sh"),
    ("cargo install the bare xwin CLI", "cargo install xwin --locked", ".sh"),
    ("brew install zig", "brew install zig", ".sh"),
    ("apt-get install zig", "apt-get install -y zig", ".sh"),
    ("choco install zig", "choco install zig -y", ".ps1"),
    ("actions-rust-cross", "      - uses: houseabsolute/actions-rust-cross@v1", ".yml"),
    ("cross-rs source install", "cargo install --git https://github.com/cross-rs/cross", ".sh"),
    ("osxcross checkout", "git clone https://github.com/tpoechtrager/osxcross", ".sh"),
    ("a checked-in Cross.toml", "cp Cross.toml /workspace/", ".sh"),
    # Rust source and Dockerfiles are build surfaces too; the empty suffix is a
    # Dockerfile or an extensionless `bash build` style entrypoint.
    ("xwin argv assembled in Rust", 'let argv = vec!["cargo", "xwin", "build"];', ".rs"),
    ("xwin install in a Dockerfile", "RUN cargo install cargo-xwin --locked", ""),
    # The multi-line GitHub Actions shape. The pre-#714 patterns were compiled
    # with re.MULTILINE but matched per line, so this could never fire.
    (
        "taiki-e/install-action with the tool on a later line",
        "      - uses: taiki-e/install-action@v2\n        with:\n          tool: cargo-xwin",
        ".yml",
    ),
    (
        "taiki-e/install-action naming cargo-zigbuild on a later line",
        "      - uses: taiki-e/install-action@v2\n        with:\n          tool: cargo-zigbuild",
        ".yml",
    ),
    (
        "a hand-rolled linker for a soldr-owned target",
        '[target.x86_64-pc-windows-msvc]\nlinker = "lld-link"',
        ".toml",
    ),
    # A quoted table key is equally idiomatic TOML, and `linker` need not come
    # first — an array value before it must not end the search window, or
    # reordering two lines defeats the rule.
    (
        "a quoted table key",
        '[target."x86_64-pc-windows-msvc"]\nlinker = "lld-link"',
        ".toml",
    ),
    (
        "linker after an array-valued key",
        '[target.x86_64-apple-darwin]\nrustflags = ["-C", "x"]\nlinker = "clang"',
        ".toml",
    ),
    # soldr#2299: zig is banned at EVERY target, including Linux. These were
    # accepted before the catalogue GNU toolchain replaced zig.
    (
        "cargo zigbuild for a linux target",
        "cargo zigbuild --target aarch64-unknown-linux-gnu",
        ".sh",
    ),
    (
        "maturin --zig for the manylinux wheel",
        "maturin build --zig --compatibility manylinux2014 "
        "--target x86_64-unknown-linux-gnu",
        ".sh",
    ),
    (
        "cargo zigbuild argv assembled in Python",
        'return ["cargo", "zigbuild", *subcommand[1:], "--target", target]',
        ".py",
    ),
    (
        "zig prose without the marker",
        "`cargo zigbuild` was the legacy Linux cross.",
        ".py",
    ),
    # A URL is a string, not a comment. Truncating the line at `//` would hide
    # a real reference to the tool.
    (
        "a cargo-xwin URL in Rust source",
        'let u = "https://github.com/rust-cross/cargo-xwin";',
        ".rs",
    ),
    (
        "an osxcross URL in Rust source",
        'const O: &str = "https://github.com/tpoechtrager/osxcross";',
        ".rs",
    ),
]


@pytest.mark.parametrize(
    ("line", "suffix"),
    [(line, suffix) for _, line, suffix in REJECTED],
    ids=[name for name, _, _ in REJECTED],
)
def test_direct_cross_tooling_is_rejected(line: str, suffix: str):
    violations = scan_text(line, suffix)
    assert violations, f"linter did not reject: {line}"


# --------------------------------------------------------------- accepted --

ACCEPTED: list[tuple[str, str, str]] = [
    (
        "soldr prepare for windows",
        "soldr prepare --target x86_64-pc-windows-msvc",
        ".sh",
    ),
    (
        "soldr build for darwin",
        "soldr build --release --target aarch64-apple-darwin",
        ".sh",
    ),
    (
        "soldr build argv assembled in Python",
        'return ["soldr", "build", *subcommand[1:], "--target", target]',
        ".py",
    ),
    # soldr#2299: zig is banned at EVERY target now, so the former "zig is the
    # supported Linux cross" accepted cases moved to REJECTED. The bare enum
    # value still names no tool, so it stays accepted.
    (
        "the matrix strategy name alone names no target",
        "      strategy: zigbuild",
        ".yml",
    ),
    # Prose explaining the ban must not trip it, or the rule becomes
    # undocumentable.
    (
        "a comment describing the forbidden command",
        "# never run `cargo xwin build --target x86_64-pc-windows-msvc` here",
        ".py",
    ),
    # Wildcard prose alone is not enough for the unconditional rules (#714,
    # soldr#2299): a banned tool name is a violation wherever it appears, target
    # or not, and a docstring is prose no comment-stripper can see. Such a line
    # must carry the marker — the honest trade, since the alternative is a rule
    # that cannot distinguish documentation from a regression.
    (
        "docstring prose naming zig with the marker",
        "`cargo zigbuild` was the legacy Linux cross. (cross-lint: allow)",
        ".py",
    ),
    (
        "docstring prose naming an unconditional tool, with the marker",
        "`cargo xwin` is the legacy passthrough. (cross-lint: allow)",
        ".py",
    ),
    (
        "plain cargo at an MSVC target is fine — clippy does not link",
        "return ['cargo', *subcommand, '--target', 'x86_64-pc-windows-msvc']",
        ".py",
    ),
    # ---------------------------------------------------------------- #714 --
    # The widened scope reaches Rust and Dockerfiles, so their comment syntax
    # has to be understood or every doc comment becomes a violation.
    (
        "a Rust line comment describing the forbidden command",
        '    // cargo-zigbuild links via zig; never `xwin splat` here',
        ".rs",
    ),
    (
        "a Dockerfile comment describing the ban",
        "# do not `cargo install cargo-xwin` in this image — use soldr",
        "",
    ),
    (
        "a PowerShell comment describing the ban",
        "# cargo xwin is banned; soldr prepare provisions the CRT",
        ".ps1",
    ),
    # Words that merely contain a banned token are not invocations.
    ("crossbeam is not the cross wrapper", "use crossbeam::channel;", ".rs"),
    ("a variable named cross_build", "let cross_build_id = 3;", ".rs"),
    # `cross` is an ordinary English word, so its rule is anchored at a command
    # position. Without that, every one of these fails `bash lint` — which is
    # how a rule earns a revert rather than a fix.
    ("cross build in a YAML description", '      description: "run a cross build"', ".yml"),
    ("cross build in a job name", "    name: cross build matrix", ".yml"),
    ("cross build inside a Rust string", '    let msg = "cross build failed";', ".rs"),
    # A Rust block comment is prose too. `.rs` is scanned now, so a doc block
    # explaining the ban must not be a violation.
    ("a Rust block comment", "/* never run cargo xwin build here */", ".rs"),
    ("a multi-line Rust block comment", "/*\n * cargo xwin build\n */", ".rs"),
    # Unrelated packages whose names merely start with the banned token.
    ("apt-get installing zlib", "apt-get install -y zlib1g-dev", ".sh"),
    ("brew installing zigbee2mqtt", "brew install zigbee2mqtt", ".sh"),
    ("an unrelated crate starting with cross", "cargo install cross-utility", ".sh"),
    # `list` and `download` are ordinary words, and a path may end in `xwin`.
    ("a path ending in xwin next to the word list", 'PATHS = ["/opt/xwin", "list"]', ".py"),
    (
        "prose mentioning xwin and download",
        'msg = "the xwin cache does not download automatically"',
        ".py",
    ),
    # Rust block comments nest, so a non-greedy `/*…*/` would leave the inner
    # text live and report something that is entirely commented out.
    (
        "text inside a nested Rust block comment",
        "/* outer\n/* inner */\ncargo xwin build\n*/\nfn main() {}",
        ".rs",
    ),
    ("a Rust doc comment", "/// never run cargo xwin build here", ".rs"),
    # A lifetime is not a char literal; treating every `'` as a delimiter
    # would desynchronise the scanner on ordinary Rust.
    ("a lifetime followed by a comment", "fn f<'a>(x: &'a str) {} // cargo xwin", ".rs"),
    ("a Rust inner doc comment", "//! cargo xwin build is banned", ".rs"),
    # The marker is read from the original line, so a trailing `//` marker is
    # not itself blanked by the comment scanner before it can be seen.
    ("a Rust trailing-comment marker", 'let t = "cargo xwin"; // cross-lint: allow', ".rs"),
    (
        "the Zig-free WASM fixture is binary data",
        "WASM = b'\\x00asm\\x01\\x00\\x00\\x00'",
        ".py",
    ),
    (
        "the WASM fixture regeneration instruction",
        "wasm-tools parse tests/fixtures/wasm/hello.wat -o tests/fixtures/wasm/hello.wasm",
        ".py",
    ),
    (
        "an install-action for an unrelated tool",
        "      - uses: taiki-e/install-action@v2\n        with:\n          tool: cargo-nextest",
        ".yml",
    ),
    # The escape hatch, for prose no comment-stripper can see.
    (
        "a line carrying the documented allow marker",
        "legacy passthrough: cargo xwin build  (cross-lint: allow)",
        ".py",
    ),
]


@pytest.mark.parametrize(
    ("line", "suffix"),
    [(line, suffix) for _, line, suffix in ACCEPTED],
    ids=[name for name, _, _ in ACCEPTED],
)
def test_supported_paths_are_accepted(line: str, suffix: str):
    violations = scan_text(line, suffix)
    assert not violations, f"linter wrongly rejected: {line} -> {violations}"


def test_the_repository_is_currently_clean():
    """The lint runs in `bash lint` and in CI's static job, so a red tree here
    is a red build. Asserting it separately means a failure names *this* rule
    rather than surfacing as a generic lint exit code."""
    assert main() == 0


def test_pyproject_is_scanned_for_ziglang():
    from ci.banned_cross_tools import _iter_files

    paths = {path.name for path in _iter_files()}
    assert "pyproject.toml" in paths


def test_the_install_action_violation_points_at_the_action_line():
    """The multi-line install patterns match across lines, so the reported line
    number comes from an offset rather than an enumerate() counter. A finding
    that pointed at line 1 of a 400-line workflow would be useless."""
    text = "\n".join(
        [
            "jobs:",
            "  build:",
            "    steps:",
            "      - uses: taiki-e/install-action@v2",
            "        with:",
            "          tool: cargo-xwin",
        ]
    )
    findings = scan_text(text, ".yml")
    # Exactly one: the suppression must span every line the install match
    # covers, or the `tool: cargo-xwin` line gets reported a second time by
    # the unconditional invocation rule — one mistake, counted twice.
    assert len(findings) == 1, f"expected exactly one finding, got {findings}"
    (number, tool, reason) = findings[0]
    assert number == 4, "must point at the install-action step, not the file head"
    assert tool == "taiki-e/install-action: cargo-xwin"
    assert "soldr prepare" in reason


def test_the_install_window_does_not_pair_distant_lines():
    """An unbounded match would pair an install-action step at the top of a
    workflow with an unrelated mention far below, which reads as a false
    accusation against whoever wrote the step.

    Since soldr#2299 every taiki-e install target (`cargo-xwin`,
    `cargo-zigbuild`) is *also* caught by an unconditional invocation rule, so
    the distant `tool:` line is flagged on its own -- that is expected. What
    this test pins is narrower: the flag must come from the invocation rule on
    the tool line, NOT from the install rule reaching down the window.
    """
    text = "\n".join(
        [
            "      - uses: taiki-e/install-action@v2",
            *["        # unrelated filler"] * 10,
            "          tool: cargo-zigbuild",
        ]
    )
    findings = scan_text(text, ".yml")
    assert not any(
        tool.startswith("taiki-e/install-action") for _, tool, _ in findings
    ), f"install rule over-reached the window: {findings}"


def test_an_install_line_reports_the_install_not_also_the_invocation():
    """`cargo install cargo-xwin` is an install, and is also — to the
    invocation rules — a mention of `cargo xwin`. Reporting both counts one
    mistake twice in the summary total and makes the fix look bigger than it
    is, so the install must suppress the invocation rule on its own line.

    Asserting the exact count rather than `len(set(...))`: the implementation
    de-duplicates, so a set-based assertion would hold for any implementation
    and prove nothing.
    """
    findings = scan_text("cargo install cargo-xwin --locked", ".sh")
    assert len(findings) == 1, f"expected exactly one finding, got {findings}"
    assert findings[0][1] == "cargo install cargo-xwin"


@pytest.mark.parametrize(
    "text",
    [
        "a\n/* x\ny */",  # block closing on the last line, no trailing newline
        "a\n/* x",  # never closed
        "a\r\n/* x */\r\nb",  # CRLF
        "/* /* */ */\nx",  # nested
        'let s = "/*";\nlet t = "*/";',  # delimiters inside strings
        "fn main() {} // trailing\n",
    ],
    ids=["eof-block", "unterminated", "crlf", "nested", "in-string", "line-comment"],
)
def test_the_rust_stripper_preserves_the_line_count(text: str):
    """Line numbers are computed from offsets into the stripped text, so a
    stripper that drops a line would misreport every finding after it. The
    scanner blanks characters rather than removing them, which makes this
    hold by construction — this test is what keeps it that way."""
    assert len(_strip_rust_comments(text).splitlines()) == len(text.splitlines())
    assert len(_strip_rust_comments(text)) == len(text), "offsets must line up too"


@pytest.mark.parametrize(
    "text",
    [
        "// page break\x0cfn main() {}\n",
        "// vertical tab\x0bhere\n",
        "// lone carriage return\rhere\n",
        "// next line\x85here\n",
        "// line separator\u2028here\n",
        "",
    ],
    ids=["form-feed", "vertical-tab", "lone-cr", "next-line", "line-separator", "empty"],
)
def test_exotic_line_breaks_do_not_crash_the_linter(text: str):
    """`str.splitlines()` breaks on eight characters, only one of which the
    comment scanner preserves — so a form feed inside a comment (an Emacs page
    break) made the two line lists differ and killed the run with a ValueError
    traceback instead of a lint message. `bash lint` and CI's static job both
    hard-failed on it."""
    scan_text(text, ".rs")


def test_the_scanner_stays_in_sync_with_every_rust_file_in_the_tree():
    """Fixtures only prove the shapes someone thought of. Raw strings desynced
    the scanner on real source for two review rounds without any fixture
    noticing, so run the invariant over every `.rs` file the linter walks.

    The assertion is `ended_clean`, not length. Length and newline count are
    preserved *unconditionally* — the scanner only ever writes a space, never
    at a newline — so asserting those passes against a scanner desynced from
    line one. A first version of this test did exactly that, and would have
    passed against the raw-string bug it was written to guard.
    """
    from ci.banned_cross_tools import _iter_files

    checked = 0
    for path in _iter_files():
        if path.suffix != ".rs":
            continue
        source = path.read_text(encoding="utf-8", errors="replace")
        stripped, ended_clean = _scan_rust(source)
        assert ended_clean, f"{path}: scanner ended mid-string or mid-comment"
        assert len(stripped) == len(source), f"{path} changed length"
        assert stripped.count("\n") == source.count("\n"), f"{path} changed line count"
        checked += 1
    assert checked > 50, f"expected the walk to reach the Rust sources, saw {checked}"


def test_the_sync_check_would_catch_a_desynced_scanner():
    """Guard for the guard: prove `ended_clean` can be false, so the test above
    is not another tautology. An unterminated string is the shape a raw-string
    misparse leaves behind."""
    assert _scan_rust('let s = "never closed')[1] is False
    assert _scan_rust("let s = /* never closed")[1] is False
    assert _scan_rust('let ok = r"\\\\?\\"; let s = "closed";')[1] is True


def test_a_tool_written_as_a_yaml_block_sequence_is_caught():
    """`tool:` may be a block sequence, which puts the name on a `- ` line —
    the one thing the step-boundary lookahead refuses to cross. Spelled with
    cargo-zigbuild because cargo-xwin would be caught by the unconditional
    rule regardless, so this would pass without the rule under test."""
    text = "\n".join(
        [
            "      - uses: taiki-e/install-action@v2",
            "        with:",
            "          tool:",
            "            - cargo-zigbuild",
        ]
    )
    assert scan_text(text, ".yml")


def test_a_tool_list_is_caught_behind_earlier_items():
    """The multi-tool list is the canonical shape, and an earlier item must
    not end the search: with only a single item reachable, `- cargo-nextest`
    above `- cargo-zigbuild` read as clean."""
    text = "\n".join(
        [
            "      - uses: taiki-e/install-action@v2",
            "        with:",
            "          tool:",
            "            - cargo-nextest",
            "            - cargo-zigbuild",
        ]
    )
    assert scan_text(text, ".yml")


@pytest.mark.parametrize(
    "items",
    [
        ['            - "cargo-zigbuild"'],
        ["            - 'cargo-zigbuild'"],
        ['            - "cargo-nextest"', "            - cargo-zigbuild"],
        [f"            - tool{n}" for n in range(6)] + ["            - cargo-zigbuild"],
    ],
    ids=["quoted", "single-quoted", "quoted-earlier-item", "seven-items"],
)
def test_quoted_and_long_tool_lists_are_caught(items: list[str]):
    """YAML permits quoting a sequence item, and a `tool:` list may be long.
    Both spellings were one character away from silence: `cargo-zigbuild` has
    no fallback rule, since the conditional rule needs a triple on the line
    and a list item names none."""
    text = "\n".join(
        ["      - uses: taiki-e/install-action@v2", "        with:", "          tool:", *items]
    )
    assert scan_text(text, ".yml")


def test_a_utf8_bom_does_not_hide_the_first_line():
    """U+FEFF is a format character, not whitespace, so a BOM sits between the
    start of the file and the first command and defeats every `^`-anchored
    rule on line 1. Windows PowerShell writes one by default — this was found
    by planting a real file, not by a fixture, because the fixtures had no way
    to acquire a BOM."""
    line = "env A=1 B=2 C=3 D=4 cross build --target x86_64-apple-darwin"
    assert scan_text(line, ".sh"), "sanity: the line is a violation"
    assert scan_text("﻿" + line, ".sh"), "a BOM must not hide it"
    assert scan_text("﻿cross build --target x86_64-apple-darwin", ".sh")


def test_an_unterminated_raw_string_reports_a_lost_sync():
    """The raw-string branch is the one whose bug motivated `ended_clean`, so
    it must not be the one branch that cannot signal a problem: running to EOF
    without a terminator means the scan lost sync."""
    assert _scan_rust('let s = r"never closed')[1] is False
    assert _scan_rust('let s = r#"never closed')[1] is False


def test_a_tool_list_item_matches_on_a_crlf_file():
    """`$` matches before `\\n`, so a trailing `\\r` sits between the tool name
    and the anchor. `Path.read_text` normalises it away in `main()`, but a
    direct `scan_text` caller — every fixture here — would see the raw text."""
    text = "\r\n".join(
        [
            "      - uses: taiki-e/install-action@v2",
            "        with:",
            "          tool:",
            "            - cargo-zigbuild",
        ]
    )
    assert scan_text(text, ".yml")


def test_an_install_step_does_not_silence_the_step_after_it():
    """An install match suppresses the invocation rules across its span, so
    the match must not reach past the end of its own YAML step. Here the
    install-action installs an unrelated tool, and the `cargo-xwin` that ends
    the match belongs to a later step — everything in between was silently
    dropped, including a genuine `xwin splat`."""
    text = "\n".join(
        [
            "      - uses: taiki-e/install-action@v2",
            "        with:",
            "          tool: cargo-nextest",
            "      - run: xwin splat --output /opt/xwin",
            "      - run: cargo-xwin build --target x86_64-pc-windows-msvc",
        ]
    )
    lines = {number for number, _, _ in scan_text(text, ".yml")}
    assert lines == {4, 5}, "both later steps are violations and must be reported"


def test_the_allow_marker_is_strictly_line_scoped():
    """A marker on a docstring's closing line does not cover a tool named three
    lines above it. Encoded because it is the mistake the marker invites — and
    the one made while adding the two markers in `tests/test_ci_matrix.py`."""
    text = "\n".join(
        [
            '    """`cargo xwin` is the legacy passthrough in soldr\'s docs;',
            "    `soldr build` is the blessed surface. Pinning this here keeps",
            '    a future edit honest. (cross-lint: allow)"""',
        ]
    )
    assert scan_text(text, ".py"), "a marker on a later line must not suppress line 1"
    assert scan_text(text, ".py")[0][0] == 1


def test_suppression_is_scoped_to_the_install_line():
    """The suppression above must not blank out a genuine invocation elsewhere
    in the same file."""
    text = "\n".join(
        [
            "cargo install cargo-xwin --locked",
            "xwin --accept-license splat --output /opt/xwin",
        ]
    )
    lines = {number for number, _, _ in scan_text(text, ".sh")}
    assert lines == {1, 2}, "both lines are mistakes and both must be reported"


def test_a_violation_reports_file_line_tool_and_the_soldr_replacement():
    """The issue asks for an actionable failure, not a boolean. A reader who
    has never seen this rule needs to know what to write instead."""
    text = "\n".join(
        [
            "steps:",
            "  - run: cargo xwin build --target x86_64-pc-windows-msvc",
        ]
    )
    (number, tool, reason) = scan_text(text, ".yml")[0]
    assert number == 2, "line number must point at the offending line"
    assert tool == "cargo xwin"
    assert "x86_64-pc-windows-msvc" in reason
    assert "soldr build --target" in reason, "must name the replacement"
    assert "docs/architecture/ci.md" in reason, "must point at the invariant"


def test_every_rule_requires_a_prefilter_needle() -> None:
    """A new rule the prefilter cannot see would silently never fire."""
    rules = (*banned_cross_tools.BANNED_ALWAYS, *banned_cross_tools.BANNED_INSTALLS)
    for tool, pattern in rules:
        source = pattern.pattern.lower()
        assert any(needle in source for needle in banned_cross_tools.PREFILTER_NEEDLES), tool


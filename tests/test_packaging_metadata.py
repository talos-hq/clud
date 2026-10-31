"""Tests for Python packaging metadata needed by local builds."""

from __future__ import annotations

import re
from pathlib import Path

import tomllib

ROOT = Path(__file__).resolve().parent.parent


def _pyproject() -> dict:
    with (ROOT / "pyproject.toml").open("rb") as handle:
        return tomllib.load(handle)


def test_declared_versions_move_in_lockstep() -> None:
    """`pyproject.toml`, the Cargo workspace, and `clud.__version__` agree.

    This exists because they did not. `src/clud/__init__.py` sat at 2.2.0
    while the other two were at 2.4.1 — five releases of drift — so
    `import clud; clud.__version__` reported a version that had not shipped in
    months. Nothing caught it, because nothing compared them.

    The Python shim is the easy one to forget: neither maturin nor cargo ever
    reads it, so a stale value breaks no build and produces no error anywhere.
    Only a human importing the package sees the wrong number.
    """
    pyproject_version = _pyproject()["project"]["version"]

    cargo_match = re.search(
        r'^version = "([^"]+)"', (ROOT / "Cargo.toml").read_text(encoding="utf-8"), re.M
    )
    assert cargo_match, "no version in Cargo.toml"

    shim_match = re.search(
        r'^__version__ = "([^"]+)"',
        (ROOT / "src" / "clud" / "__init__.py").read_text(encoding="utf-8"),
        re.M,
    )
    assert shim_match, "no __version__ in src/clud/__init__.py"

    assert pyproject_version == cargo_match.group(1) == shim_match.group(1), (
        "version drift: "
        f"pyproject.toml={pyproject_version}, "
        f"Cargo.toml={cargo_match.group(1)}, "
        f"clud.__version__={shim_match.group(1)}"
    )


def _backend_soldr_policy() -> str:
    """The rolling soldr policy used by the PEP 517 backend."""
    build_system = _pyproject()["build-system"]
    requirements = [
        requirement.lower().replace(" ", "") for requirement in build_system["requires"]
    ]

    assert len(requirements) == 1, f"expected exactly one build requirement, got {requirements}"
    assert requirements == ["soldr"], (
        f"build-system.requires must use the rolling soldr release policy, got {requirements!r}"
    )
    return "latest"


def _setup_soldr_sources() -> list[Path]:
    """Every workflow or composite action that can contain setup-soldr."""
    workflows = ROOT / ".github" / "workflows"
    actions = ROOT / ".github" / "actions"
    paths = [
        *workflows.glob("*.yml"),
        *workflows.glob("*.yaml"),
        *actions.glob("*/action.yml"),
        *actions.glob("*/action.yaml"),
    ]
    return sorted(
        path for path in set(paths) if "forge-ci/setup-soldr" in path.read_text(encoding="utf-8")
    )


def _action_input_default(source_name: str, text: str, input_name: str) -> str:
    """Resolve a composite action's literal top-level input default."""
    lines = text.splitlines()
    input_header = f"  {input_name}:"
    input_index = next(
        (index for index, line in enumerate(lines) if line == input_header),
        None,
    )
    assert input_index is not None, (
        f"{source_name}: setup-soldr version uses inputs.{input_name}, "
        "but that input is not declared"
    )

    for candidate in lines[input_index + 1 :]:
        if candidate and not candidate.startswith(" "):
            break
        if candidate.startswith("  ") and not candidate.startswith("    "):
            break
        match = re.fullmatch(
            r"""    default:\s*(?P<quote>["']?)(?P<version>[\d.]+)(?P=quote)\s*""",
            candidate,
        )
        if match is not None:
            return match.group("version")

    raise AssertionError(
        f"{source_name}: inputs.{input_name} needs a literal version default "
        "so it can be checked against the build-backend pin"
    )


def _setup_soldr_steps_in_text(  # noqa: C901
    source_name: str, text: str
) -> list[tuple[str, str, str]]:
    """Parse setup-soldr steps and resolve a composite input's default."""
    steps: list[tuple[str, str, str]] = []
    lines = text.splitlines()
    uses_pattern = re.compile(
        r"^(?P<indent> *)(?P<dash>-\s+)?uses:\s*"
        r"forge-ci/setup-soldr@(?P<ref>\S+)\s*$"
    )

    for index, line in enumerate(lines):
        uses = uses_pattern.fullmatch(line)
        if uses is None:
            continue

        content_indent = len(uses.group("indent")) + len(uses.group("dash") or "")
        step_indent = content_indent - 2
        assert step_indent >= 0, f"{source_name}: malformed setup-soldr step indentation"

        step_end = len(lines)
        for candidate_index in range(index + 1, len(lines)):
            candidate = lines[candidate_index]
            stripped = candidate.lstrip()
            if not stripped or stripped.startswith("#"):
                continue
            indent = len(candidate) - len(stripped)
            if indent <= step_indent:
                step_end = candidate_index
                break

        with_index = next(
            (
                candidate_index
                for candidate_index in range(index + 1, step_end)
                if lines[candidate_index].strip() == "with:"
                and len(lines[candidate_index]) - len(lines[candidate_index].lstrip())
                == content_indent
            ),
            None,
        )

        version: str | None = None
        if with_index is not None:
            for candidate in lines[with_index + 1 : step_end]:
                stripped = candidate.lstrip()
                if not stripped or stripped.startswith("#"):
                    continue
                indent = len(candidate) - len(stripped)
                if indent <= content_indent:
                    break
                if indent != content_indent + 2:
                    continue
                literal = re.fullmatch(
                    r"""version:\s*(?P<quote>["']?)(?P<version>[\d.]+)(?P=quote)\s*""",
                    stripped,
                )
                if literal is not None:
                    version = literal.group("version")
                    break
                forwarded = re.fullmatch(
                    r"version:\s*\$\{\{\s*inputs\.(?P<input>[\w-]+)\s*\}\}\s*",
                    stripped,
                )
                if forwarded is not None:
                    version = _action_input_default(source_name, text, forwarded.group("input"))
                    break

        version = version or "latest"
        steps.append((source_name, uses.group("ref"), version))
    return steps


def _setup_soldr_steps() -> list[tuple[str, str, str]]:
    """Every `forge-ci/setup-soldr` step as (workflow name, action ref, version).

    Parsed without a YAML dependency, but indentation still matters. A version
    must either be literal or forwarded from a composite-action input with a
    literal default. An unrelated ``env.version`` must not satisfy the guard.
    """
    steps: list[tuple[str, str, str]] = []
    for path in _setup_soldr_sources():
        text = path.read_text(encoding="utf-8")
        source_name = path.relative_to(ROOT).as_posix()
        steps.extend(_setup_soldr_steps_in_text(source_name, text))
    return steps


def test_setup_soldr_parser_rejects_version_outside_with() -> None:
    workflow = """
jobs:
  test:
    steps:
      - uses: forge-ci/setup-soldr@v0.9.66
        env:
          version: 0.8.28
        with:
          cache: true
"""
    assert _setup_soldr_steps_in_text("bad.yml", workflow) == [("bad.yml", "v0.9.66", "latest")]


def test_pip_build_uses_soldr_pep517_backend() -> None:
    build_system = _pyproject()["build-system"]

    assert build_system["build-backend"] == "soldr"
    assert "backend-path" not in build_system
    # The backend follows the same rolling release policy as setup-soldr.
    assert _backend_soldr_policy() == "latest"


def _install_script_soldr() -> str:
    """The soldr version `./install` puts on a developer's PATH by default."""
    text = (ROOT / "install").read_text(encoding="utf-8")
    match = re.search(r'^VERSION="\$\{SOLDR_VERSION:-(latest|[\d.]+)\}"', text, re.M)
    assert match, 'install: no rolling `VERSION="${SOLDR_VERSION:-latest}"` default found'
    return match.group(1)


def test_soldr_release_policy_moves_in_lockstep() -> None:
    """The build-backend pin, every CI toolchain pin, and `./install` agree.

    The PEP 517 backend, CI toolchain, and developer installer resolve Soldr
    independently. Their defaults must all remain rolling so one path cannot
    fossilize on an older protocol while the others advance.
    """
    steps = _setup_soldr_steps()
    assert steps, "no forge-ci/setup-soldr steps found in .github"

    expected = _backend_soldr_policy()
    drift = [(name, version) for name, _, version in steps if version != expected]
    installed = _install_script_soldr()
    if installed != expected:
        drift.append(("install", installed))

    assert not drift, (
        f"soldr release-policy drift: pyproject.toml uses {expected}, but "
        + ", ".join(f"{name} uses {version}" for name, version in drift)
    )

    # The setup-soldr *action* floats on the major tag (v0): it is stable and
    # decoupled from the soldr *tool* version, which is what the lockstep above
    # enforces. All refs must still agree with each other.
    action_pins = {ref for _, ref, _ in steps}
    assert action_pins == {"v0"}, f"setup-soldr action refs disagree: {sorted(action_pins)}"


def test_install_script_uses_wheel_with_legacy_fallback() -> None:
    """`./install` must handle the asset format the pinned release ships.

    soldr 0.8.x stopped publishing `.tar.gz`/`.zip` and ships `.tar.zst`,
    which needs a `zstd` binary the script cannot assume (git-bash on Windows
    has GNU tar but no zstd, so even `tar --zstd` fails with "Cannot exec").
    The script therefore prefers the release's wheel — a plain zip carrying
    the same binary — and keeps the legacy archive only as a fallback for
    0.7.x, which published no wheels.

    Without this, the lockstep test above is satisfiable by a script that
    404s on every download.
    """
    text = (ROOT / "install").read_text(encoding="utf-8")

    assert 'wheel_asset="soldr-${VERSION}-py3-none-${wheel_tag}.whl"' in text, (
        "install must resolve the wheel asset; the .tar.zst archives soldr "
        "0.8.x publishes cannot be extracted without a zstd binary"
    )
    assert "legacy_asset=" in text, "install must keep the 0.7.x archive fallback"
    assert 'if [[ "$VERSION" == 0.7.* ]]; then' in text, (
        "only soldr 0.7.x should use the legacy archive path"
    )
    assert '"$target_dir/python.exe" -c' in text
    assert '"$target_dir/python" -c' in text
    assert "command -v python3" in text  # asserts the installer's fallback; python-name-lint: allow


def test_ci_setup_soldr_cooks_only_with_the_build_target() -> None:
    """The dependency cook runs inside the setup-soldr step, before
    `Prepare cross toolchain` exports the blessed toolchain env. That is only
    correct because `soldr cook --target X` applies the same blessed prep as
    `soldr build --target X` (zackees/soldr#3858, soldr 0.9.39): a cook
    without `--target` is a host cook whose units the build rebuilds, which is
    why clud ran with `prebuild-deps: none` until #2049. So the build lane
    cooks, and only ever with its own target and the build's profile; every
    other setup-soldr consumer stays off.
    """
    build_setup = ROOT / ".github" / "actions" / "setup-build" / "action.yml"
    text = build_setup.read_text(encoding="utf-8")
    assert "prebuild-deps: ${{ inputs.cook == 'true' && 'soldr-cook' || 'none' }}" in text
    # A lint-only call checks dependencies and reuses no cooked unit (#2071).
    reusable = (ROOT / ".github" / "workflows" / "_build-target.yml").read_text(encoding="utf-8")
    assert "cook: ${{ inputs.compile }}" in reusable
    assert (
        "prebuild-deps-flags: --target ${{ inputs.target }} --workspace"
        "${{ inputs.profile == 'release' && ' --release' || '' }}"
    ) in text
    for path in _setup_soldr_sources():
        if path == build_setup:
            continue
        assert "prebuild-deps: soldr-cook" not in path.read_text(encoding="utf-8"), path


def test_soldr_workflows_can_quarantine_yanked_dependency_caches() -> None:
    """Every entrypoint that reaches setup-soldr grants exact-cache deletion."""
    for relative in (
        ".github/workflows/ci.yml",
        ".github/workflows/_dylint.yml",
        ".github/workflows/auto-release.yml",
    ):
        text = (ROOT / relative).read_text(encoding="utf-8")
        permissions = text.split("permissions:", 1)[1].split("\njobs:", 1)[0]
        assert "contents: read" in permissions, relative
        assert "actions: write" in permissions, relative


def test_ci_setup_soldr_cook_profile_matches_the_real_build() -> None:
    setup = ROOT / ".github" / "actions" / "setup-build" / "action.yml"
    build = ROOT / ".github" / "workflows" / "_build-target.yml"

    setup_text = setup.read_text(encoding="utf-8")
    build_text = build.read_text(encoding="utf-8")
    assert "${{ inputs.profile == 'release' && ' --release' || '' }}" in setup_text
    assert "profile: ${{ inputs.profile }}" in build_text


def test_mcp_sdk_is_a_base_dependency() -> None:
    # Hermes runs `python -m clud.mcp_server` from a plain `uv tool install
    # clud`; an opt-in extra was dropped by reinstalls and broke it silently.
    project = _pyproject()["project"]
    mcp = [d for d in project["dependencies"] if d.startswith("mcp")]
    assert mcp == ["mcp>=2.1; sys_platform != 'win32' or platform_machine != 'ARM64'"]
    # The extra stays as an empty alias so `clud[mcp]` still resolves.
    assert project["optional-dependencies"]["mcp"] == []


def test_setup_soldr_uses_its_current_repository() -> None:
    """setup-soldr moved to forge-ci; the old name works only by redirect (#2419)."""
    stale = [
        f"{path.relative_to(ROOT)}:{number}"
        for path in (ROOT / ".github").rglob("*.y*ml")
        for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1)
        if "uses:" in line and "zackees/setup-soldr@" in line
    ]
    assert not stale, f"repin to forge-ci/setup-soldr: {stale}"


def test_setup_soldr_cache_steps_wire_the_s3_remote_cache() -> None:
    """Every `cache: true` setup-soldr step gets the S3 cache, and its keys arrive.

    #2462: bucket/endpoint as inputs, credentials as step env. Reusable
    workflows get no secrets by default, so every caller that reaches such a
    step must pass `secrets: inherit`; the composite setup-build action cannot
    read secrets, so each of its callers forwards them as inputs.
    """
    workflows = ROOT / ".github" / "workflows"
    for path in _setup_soldr_sources():
        text = path.read_text(encoding="utf-8")
        name = path.relative_to(ROOT).as_posix()
        for block in re.split(r"\n\s*- (?:name: .*\n\s*(?:if: .*\n\s*)?)?uses: ", text)[1:]:
            if not block.startswith("forge-ci/setup-soldr@") or "cache: true" not in block:
                continue
            assert "s3-bucket: forge-ci-cache" in block, name
            assert "s3-endpoint: sfo3.digitaloceanspaces.com" in block, name
            for key in ("SOLDR_S3_CACHE_ACCESS_KEY", "SOLDR_S3_CACHE_SECRET_KEY"):
                assert f"{key}: ${{{{" in block, f"{name}: {key} not exported"

    # Reusable workflows that (transitively) reach a cache step.
    reaching = {"_build-target.yml", "_dylint.yml", "ci.yml", "installer-check.yml"}
    for path in workflows.glob("*.yml"):
        text = path.read_text(encoding="utf-8")
        for match in re.finditer(
            r"^    uses: \./\.github/workflows/(\S+)\n((?:    #.*\n)*)(    \S.*)?$", text, re.M
        ):
            if match.group(1) not in reaching:
                continue
            job = text[match.start() :].split("\n\n  ")[0]
            assert re.search(r"^    secrets: inherit$", job, re.M), (
                f"{path.name}: caller of {match.group(1)} lacks `secrets: inherit`"
            )
        for match in re.finditer(
            r"uses: \./\.github/actions/setup-build\n((?:\s+.*\n){1,12})", text
        ):
            assert "s3-access-key: ${{ secrets.SOLDR_S3_CACHE_ACCESS_KEY }}" in match.group(1), (
                path.name
            )
            assert "s3-secret-key: ${{ secrets.SOLDR_S3_CACHE_SECRET_KEY }}" in match.group(1), (
                path.name
            )

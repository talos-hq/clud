"""installer/homebrew_formula.py renders talos-hq/homebrew-tap's Formula/clud.rb."""

import pytest

from installer import homebrew_formula as hf

TRIPLES = [triple for _, _, triple in hf.TARGETS]


def sums(version: str, prefix: str = "./", skip: str | None = None) -> str:
    lines = [
        f"{str(index) * 64}  {prefix}clud-{version}-{triple}"
        for index, triple in enumerate(TRIPLES, start=1)
        if triple != skip
    ]
    lines.append(f"{'e' * 64}  ./clud-{version}-x86_64-pc-windows-msvc.exe")
    return "\n".join(lines) + "\n"


@pytest.mark.parametrize("tag", ["2.8.65", "v2.8.65"])
def test_formula_pins_every_platform_asset_by_digest(tag) -> None:
    text = hf.render(tag, sums("2.8.65"))
    assert "class Clud < Formula" in text
    # brew audit --strict: the version is scanned from the URL, never repeated.
    assert "  version " not in text
    for index, triple in enumerate(TRIPLES, start=1):
        url = f"https://github.com/talos-hq/clud/releases/download/{tag}/clud-2.8.65-{triple}"
        block = text.split(f'url "{url}"', 1)[1]
        assert block.lstrip().startswith(f'sha256 "{str(index) * 64}"'), triple
    assert ".exe" not in text


def test_formula_nests_cpu_blocks_inside_os_blocks() -> None:
    text = hf.render("2.8.62", sums("2.8.62"))
    macos = text.split("  on_macos do\n", 1)[1].split("\n  end\n", 1)[0]
    linux = text.split("  on_linux do\n", 1)[1].split("\n  end\n", 1)[0]
    assert "apple-darwin" in macos
    assert "linux-musl" not in macos
    assert "linux-musl" in linux
    assert "apple-darwin" not in linux
    for block in (macos, linux):
        assert "on_arm do" in block
        assert "on_intel do" in block


@pytest.mark.parametrize("prefix", ["./", "*", ""])
def test_sha256sums_name_prefixes_are_accepted(prefix) -> None:
    assert hf.render("1.2.3", sums("1.2.3", prefix=prefix)).count('sha256 "') == 4


@pytest.mark.parametrize("triple", TRIPLES)
def test_a_missing_platform_asset_refuses_to_render(triple) -> None:
    with pytest.raises(ValueError, match=triple):
        hf.render("2.8.62", sums("2.8.62", skip=triple))


@pytest.mark.parametrize(
    "version", ["vv2.8.62", "2.8", "2.8.62-rc1", "v2.8.62-rc1", "", "2.8.62\n"]
)
def test_only_stable_release_versions_render(version) -> None:
    with pytest.raises(ValueError, match="stable release version"):
        hf.render(version, sums("2.8.62"))


def test_install_renames_the_single_downloaded_asset_to_clud() -> None:
    text = hf.render("2.8.62", sums("2.8.62"))
    assert 'bin.install Dir["clud-*"].fetch(0) => "clud"' in text
    assert 'shell_output("#{bin}/clud --version")' in text


@pytest.mark.parametrize("tag", ["2.8.62", "v2.8.62"])
def test_cli_writes_the_formula_and_reports_errors(tag, tmp_path, capsys) -> None:
    good = tmp_path / "SHA256SUMS"
    good.write_text(sums("2.8.62"), encoding="utf-8")
    assert hf.main([tag, str(good)]) == 0
    assert f"releases/download/{tag}/clud-2.8.62-" in capsys.readouterr().out
    assert hf.main(["2.8.62", str(tmp_path / "missing")]) == 1
    assert hf.main(["2.8.62"]) == 2

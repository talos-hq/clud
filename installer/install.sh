#!/bin/sh
# Install the native clud binary on macOS or Linux.
#
#   curl -fsSL https://clud.bot/install.sh | sh
#   curl -fsSL https://clud.bot/install.sh | sh -s -- --dry-run
#
# Flags:
#   --dry-run          print what would be fetched and where it would go; download nothing
#   --version <tag>    install this release tag instead of the latest release
#   --write-profile    append the install directory to ~/.profile when it is not on PATH
#   -h, --help         show this help
#
# Environment:
#   CLUD_INSTALL_DIR   install directory (default: $HOME/.local/bin)
#
# The binary is verified against the release's SHA256SUMS before it is
# installed; a mismatch or a missing entry aborts. Windows is not handled
# here: download the .exe from https://talosbot.ai/clud/.

set -eu

REPO="talos-hq/clud"
SUPPORTED="Linux x86_64, Linux aarch64, macOS x86_64, macOS aarch64 (Apple Silicon)"

say() { printf '%s\n' "$*"; }
die() { printf 'clud install: error: %s\n' "$*" >&2; exit 1; }

usage() {
    say "usage: install.sh [--dry-run] [--version <tag>] [--write-profile]"
    say "  --dry-run        print what would be fetched and where it would go; download nothing"
    say "  --version <tag>  install this release tag instead of the latest release"
    say "  --write-profile  append the install directory to ~/.profile when it is not on PATH"
    say "  CLUD_INSTALL_DIR overrides the install directory (default: \$HOME/.local/bin)"
}

dry_run=0
version=""
write_profile=0
while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) dry_run=1 ;;
        --write-profile) write_profile=1 ;;
        --version)
            [ $# -ge 2 ] || die "--version needs a release tag (for example --version 2.8.56)"
            version="$2"
            shift
            ;;
        --version=*) version="${1#--version=}" ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown option: $1 (supported: --dry-run, --version <tag>, --write-profile, --help)" ;;
    esac
    shift
done

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
    Linux) suffix="unknown-linux-musl" ;;
    Darwin) suffix="apple-darwin" ;;
    *) die "unsupported platform: $os $arch. Supported: $SUPPORTED. On Windows, download the .exe from https://talosbot.ai/clud/" ;;
esac
case "$arch" in
    x86_64|amd64) arch="x86_64" ;;
    aarch64|arm64) arch="aarch64" ;;
    *) die "unsupported architecture: $os $arch. Supported: $SUPPORTED" ;;
esac
# An Intel shell under Rosetta on Apple Silicon still wants the native binary.
if [ "$os" = Darwin ] && [ "$arch" = x86_64 ] &&
    [ "$(sysctl -n sysctl.proc_translated 2>/dev/null || true)" = 1 ]; then
    arch="aarch64"
fi

if [ -z "$version" ]; then
    latest="$(curl -fsSL -I -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest")" ||
        die "could not resolve the latest release of $REPO"
    version="${latest##*/tag/}"
    [ -n "$version" ] && [ "$version" != "$latest" ] ||
        die "could not resolve the latest release tag from $latest"
fi

asset="clud-$version-$arch-$suffix"
base="https://github.com/$REPO/releases/download/$version"
dir="${CLUD_INSTALL_DIR:-$HOME/.local/bin}"
dest="$dir/clud"

if [ "$dry_run" = 1 ]; then
    say "clud install (dry run): nothing will be downloaded or written"
    say "  platform:  $os $arch"
    say "  release:   $version"
    say "  download:  $base/$asset"
    say "  checksums: $base/SHA256SUMS"
    say "  install:   $dest"
    exit 0
fi

tmp="$(mktemp -d "${TMPDIR:-/tmp}/clud-install.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT INT TERM

say "Downloading $asset ($version)"
curl -fsSL -o "$tmp/$asset" "$base/$asset" || die "download failed: $base/$asset"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" || die "download failed: $base/SHA256SUMS"

expected="$(awk -v name="$asset" '$2 == name || $2 == "./" name || $2 == "*" name { print $1; exit }' "$tmp/SHA256SUMS")"
[ -n "$expected" ] || die "$asset is not listed in the release's SHA256SUMS; refusing to install"
if command -v sha256sum >/dev/null 2>&1; then
    actual="$(sha256sum "$tmp/$asset" | awk '{ print $1 }')"
elif command -v shasum >/dev/null 2>&1; then
    actual="$(shasum -a 256 "$tmp/$asset" | awk '{ print $1 }')"
else
    die "neither sha256sum nor shasum is available to verify the download"
fi
[ "$actual" = "$expected" ] ||
    die "checksum mismatch for $asset: SHA256SUMS says $expected, download is $actual; refusing to install"

mkdir -p "$dir"
chmod 755 "$tmp/$asset"
mv -f "$tmp/$asset" "$dest"
say "Installed clud $version to $dest (sha256 verified)"

case ":${PATH}:" in
    *":$dir:"*) ;;
    *)
        line="export PATH=\"$dir:\$PATH\""
        if [ "$write_profile" = 1 ]; then
            profile="$HOME/.profile"
            if ! grep -qsF "$line" "$profile"; then
                printf '\n# Added by the clud installer\n%s\n' "$line" >> "$profile"
            fi
            say "Added $dir to PATH in $profile; open a new shell to pick it up."
        else
            say "$dir is not on PATH. Add it with:"
            say "  $line"
            say "(or re-run with --write-profile to append that line to ~/.profile)"
        fi
        ;;
esac

say "Next step: run 'clud setup'"

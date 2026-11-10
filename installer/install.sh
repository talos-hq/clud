#!/bin/sh
# Install the native clud binary on macOS or Linux.
#
#   curl -fsSL https://clud.bot/install.sh | sh
#   curl -fsSL https://clud.bot/install.sh | sh -s -- --dry-run
#
# Flags:
#   --dry-run          print what would be fetched and where it would go; download nothing
#   --version <tag>    install this release tag instead of the latest release
#   --write-profile    activate the install directory in your shell's startup files without asking
#   --no-modify-path   never edit a shell startup file; only print how to activate clud
#   --no-setup         do not offer to run `clud setup` after installing
#   -h, --help         show this help
#
# Environment:
#   CLUD_INSTALL_DIR   install directory (default: $HOME/.local/bin)
#   NO_COLOR           print the next step without colour
#
# After installing, when the new binary reports that setup has not been run
# (`clud setup --check` exits 3) and a terminal is available, the installer
# asks "Set up clud now? [y/n]" on the terminal (stdin is the piped script)
# and runs the installed binary's `clud setup` by its full path, so it works
# before the install directory is on PATH.
#
# Installing and PATH activation belong to the verified binary itself
# (`clud --installer --install-current`): when the install directory is not on
# PATH it shows the exact startup-file edits for your login shell (zsh:
# ~/.zprofile and ~/.zshrc, which is what macOS needs), asks on the terminal,
# writes a marker-delimited block, checks a fresh shell finds clud, and tells
# you to source the file or open a new terminal. With no terminal it installs
# without touching any startup file and prints the activation command.
#
# The binary is verified against the release's SHA256SUMS before it is
# installed; a mismatch or a missing entry aborts. Windows is not handled
# here: download the .exe from https://talos-hq.github.io/clud/.

set -eu

REPO="talos-hq/clud"
SUPPORTED="Linux x86_64, Linux aarch64, macOS x86_64, macOS aarch64 (Apple Silicon)"

say() { printf '%s\n' "$*"; }
die() { printf 'clud install: error: %s\n' "$*" >&2; exit 1; }

usage() {
    say "usage: install.sh [--dry-run] [--version <tag>] [--write-profile | --no-modify-path]"
    say "  --dry-run        print what would be fetched and where it would go; download nothing"
    say "  --version <tag>  install this release tag instead of the latest release"
    say "  --write-profile  activate the install directory in your shell's startup files without asking"
    say "  --no-modify-path never edit a shell startup file; only print how to activate clud"
    say "  --no-setup       do not offer to run 'clud setup' after installing"
    say "  CLUD_INSTALL_DIR overrides the install directory (default: \$HOME/.local/bin)"
}

dry_run=0
version=""
write_profile=ask
offer_setup=1
while [ $# -gt 0 ]; do
    case "$1" in
        --dry-run) dry_run=1 ;;
        --write-profile) write_profile=yes ;;
        --no-modify-path) write_profile=no ;;
        --no-setup) offer_setup=0 ;;
        --version)
            [ $# -ge 2 ] || die "--version needs a release tag (for example --version 2.8.56)"
            version="$2"
            shift
            ;;
        --version=*) version="${1#--version=}" ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown option: $1 (supported: --dry-run, --version <tag>, --write-profile, --no-modify-path, --no-setup, --help)" ;;
    esac
    shift
done

os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
    Linux) suffix="unknown-linux-musl" ;;
    Darwin) suffix="apple-darwin" ;;
    *) die "unsupported platform: $os $arch. Supported: $SUPPORTED. On Windows, download the .exe from https://talos-hq.github.io/clud/" ;;
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

# Asset names never carry the tag's "v" (v2.8.65 ships clud-2.8.65-*, #2786).
asset="clud-${version#v}-$arch-$suffix"
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

chmod 755 "$tmp/$asset"

# `curl ... | sh` makes the script itself stdin, so questions go to the
# terminal. CLUD_INSTALL_TTY overrides the device; empty means none. The probe
# runs in a subshell: a failed redirection on `:` would exit a POSIX sh.
tty="${CLUD_INSTALL_TTY-/dev/tty}"
has_tty() { [ -n "$tty" ] && (: <"$tty") 2>/dev/null; }

# Hand the install and PATH activation to the verified binary. Activation is
# only planned when the directory is missing from PATH, never without a
# terminal unless --write-profile asked for it. --installer-json keeps the
# provider step for the setup offer below and reports the outcome on stdout.
modify_path=1
case ":${PATH}:" in
    *":$dir:"*) modify_path=0 ;;
esac
[ "$write_profile" = no ] && modify_path=0
interactive=0
if [ "$write_profile" != yes ] && has_tty; then interactive=1; fi
# Without a terminal nothing may ask, so nothing is activated either.
if [ "$interactive" = 0 ] && [ "$write_profile" != yes ]; then modify_path=0; fi
set -- --installer --install-current --installer-json
[ "$interactive" = 1 ] || set -- "$@" --yes
[ "$modify_path" = 1 ] || set -- "$@" --no-modify-path
export CLUD_INSTALL_DIR="$dir"
status=0
if [ "$interactive" = 1 ]; then
    "$tmp/$asset" "$@" <"$tty" >"$tmp/outcome.json" || status=$?
else
    "$tmp/$asset" "$@" </dev/null >"$tmp/outcome.json" || status=$?
fi
if ! grep -q '"binary_committed":true' "$tmp/outcome.json" 2>/dev/null; then
    [ "$status" = 0 ] || die "the clud installer did not finish (exit $status); nothing was installed"
    say "clud was not installed."
    exit 0
fi
[ "$status" = 0 ] ||
    say "clud is installed, but shell activation did not finish (exit $status); see the message above."
say "Installed clud $version to $dest (sha256 verified)"

# Green when printing to a colour terminal (NO_COLOR opts out).
green() {
    if { [ -t 1 ] || [ "${CLUD_INSTALL_COLOR:-}" = always ]; } && [ -z "${NO_COLOR:-}" ]; then
        printf '\033[32m%s\033[0m\n' "$*"
    else
        say "$*"
    fi
}

if [ "$offer_setup" = 1 ] && has_tty && { "$dest" setup --check >/dev/null 2>&1; [ $? -eq 3 ]; }; then
    exec 3<"$tty"
    printf 'clud is not set up yet. Set up clud now? [y/n] '
    answer=""
    read -r answer <&3 || answer=""
    case "$answer" in
        y|Y|yes|Yes|YES)
            "$dest" setup <&3 3<&- || die "clud setup did not finish; run it again with: $dest setup"
            exit 0
            ;;
    esac
    exec 3<&-
fi
green "Next step: run 'clud setup'"

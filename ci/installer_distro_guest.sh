#!/bin/sh
set -eu

distro=$1
version=$2
expected_sha=$3
guest_arch=$4
case "$guest_arch" in x86_64|aarch64) ;; *) exit 2 ;; esac
test "$(uname -m)" = "$guest_arch"
case "$distro" in
  ubuntu|debian)
    apt-get update
    DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends bash passwd coreutils util-linux
    shell=/bin/bash
    ;;
  archlinux)
    pacman -Syu --noconfirm fish shadow
    shell=/usr/bin/fish
    ;;
  fedora)
    dnf install -y shadow-utils util-linux-user bash coreutils
    shell=/bin/bash
    ;;
  alpine)
    apk add --no-cache bash shadow coreutils
    shell=/bin/bash
    ;;
  *) exit 2 ;;
esac
useradd -m -s "$shell" alice
cp "/candidate/clud-$version-$guest_arch-unknown-linux-musl" /home/alice/clud-candidate
chmod 755 /home/alice/clud-candidate
chown alice:alice /home/alice/clud-candidate
actual_sha=$(sha256sum /home/alice/clud-candidate | cut -d ' ' -f 1)
test "$actual_sha" = "$expected_sha"
su - alice -c 'env CLUD_INSTALLER_CI_FIXTURE_DIR=/fixture HTTPS_PROXY=http://127.0.0.1:1 /home/alice/clud-candidate --installer --install-current --yes'
if [ "$distro" = archlinux ]; then
  selected=$(su - alice -c 'fish -lc "command -v clud; clud --version"')
else
  selected=$(su - alice -c 'bash -lc "command -v clud; clud --version"')
fi
expected_output="/home/alice/.local/bin/clud
clud $version"
test "$selected" = "$expected_output"
installed_sha=$(sha256sum /home/alice/.local/bin/clud | cut -d ' ' -f 1)
test "$installed_sha" = "$expected_sha"
printf 'DISTRO_EVIDENCE distro=%s host_arch=%s payload_arch=%s sha256=%s resolved_path=/home/alice/.local/bin/clud version=%s\n' "$distro" "$guest_arch" "$guest_arch" "$installed_sha" "$version"

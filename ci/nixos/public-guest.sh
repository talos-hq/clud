#!/bin/bash
set -euo pipefail

mode=$1
tag=$2
version=$3
expected_sha=$4
expected_arch=$5
nix_ld=$6
test "$(uname -m)" = "$expected_arch"
case "$nix_ld" in
  disabled)
    test ! -e /lib64/ld-linux-x86-64.so.2
    test ! -e /lib/ld-linux-aarch64.so.1
    ;;
  enabled)
    test -x /lib64/ld-linux-x86-64.so.2 || test -x /lib/ld-linux-aarch64.so.1
    ;;
  *) exit 2 ;;
esac

if [ "$mode" = candidate ]; then
  catalog_url="https://github.com/zackees/clud/releases/download/$tag/installer-candidate-manifest.json"
  channel=candidate
elif [ "$mode" = released ]; then
  catalog_url=https://talos-hq.github.io/clud/install/manifest.json
  channel=latest-stable
else
  exit 2
fi
asset_url="https://github.com/zackees/clud/releases/download/$tag/clud-$version-$expected_arch-unknown-linux-musl"
curl -fLsS --retry 5 --retry-all-errors "$catalog_url" -o /tmp/catalog.json
curl -fLsS --retry 5 --retry-all-errors "$asset_url" -o /tmp/clud-public
test "$(jq -r --arg channel "$channel" '.channels[$channel]' /tmp/catalog.json)" = "$version"
catalog_sha=$(jq -r --arg version "$version" --arg arch "$expected_arch" '
  .releases[] | select(.version == $version) | .platforms[] |
  select(.platform.os == "linux" and .platform.arch == $arch and .variant.flavor == "static-musl") |
  .asset.sha256
' /tmp/catalog.json)
test "$catalog_sha" = "$expected_sha"
test "$(sha256sum /tmp/clud-public | cut -d ' ' -f 1)" = "$expected_sha"
if readelf -l /tmp/clud-public | grep -q INTERP; then
  echo 'public musl binary has an ELF interpreter' >&2
  exit 1
fi
install -m 755 -o alice -g users /tmp/clud-public /home/alice/clud-public
if [ "$mode" = candidate ]; then
  su - alice -c "CLUD_INSTALLER_CANDIDATE_TAG=$tag /home/alice/clud-public --installer --install-version $version --yes"
else
  su - alice -c "/home/alice/clud-public --installer --install-version $version --yes"
fi
selected=$(su - alice -c 'bash -lc "command -v clud; clud --version"')
expected_output="/home/alice/.local/bin/clud
clud $version"
test "$selected" = "$expected_output"
test "$(sha256sum /home/alice/.local/bin/clud | cut -d ' ' -f 1)" = "$expected_sha"
jq -nc --arg mode "$mode" --arg tag "$tag" --arg arch "$expected_arch" \
  --arg sha "$expected_sha" --arg version "$version" --arg nix_ld "$nix_ld" \
  '{mode:$mode,tag:$tag,host_arch:$arch,nix_ld:$nix_ld,sha256:$sha,version:$version,resolved_path:"/home/alice/.local/bin/clud"}' |
  sed 's/^/PUBLIC_NIXOS_EVIDENCE /'

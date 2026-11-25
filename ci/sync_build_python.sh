#!/usr/bin/env bash
set -euo pipefail

sync=(uv sync --frozen --group dev --no-install-project --python 3.11)

unpack_wheels() {
  python - "$1" "$2" <<'PY'
import sys
import tarfile
from pathlib import Path

with tarfile.open(sys.argv[1], "r:") as archive:
    wheels = archive.getmembers()
    if not wheels or any(
        wheel.type != tarfile.REGTYPE or Path(wheel.name).name != wheel.name
        or not wheel.name.endswith(".whl") for wheel in wheels
    ):
        raise ValueError("wheelhouse contains an invalid entry")
    archive.extractall(sys.argv[2], members=wheels, filter="data")
PY
}

publish_wheels() {
  python - "$1" "$2" <<'PY'
import os
import sys
import tarfile
from pathlib import Path
from tempfile import NamedTemporaryFile

wheels, asset = map(Path, sys.argv[1:])
with NamedTemporaryFile(dir=asset.parent, prefix=".clud-wheels-", delete=False) as stream:
    staging = Path(stream.name)
    try:
        with tarfile.open(fileobj=stream, mode="w") as archive:
            for wheel in sorted(wheels.glob("*.whl")):
                archive.add(wheel, arcname=wheel.name)
        stream.flush()
        staging.chmod(0o644)
        os.replace(staging, asset)
    finally:
        staging.unlink(missing_ok=True)
PY
}

install_wheels() {
  # Read and authenticate the actual wheel bytes without a shared uv cache.
  uv pip sync --no-cache --offline --no-index --find-links "$1" \
    --only-binary=:all: --require-hashes --python .venv/bin/python "$2"
}

sync_from_wheelhouse() (
  local assets=$1 work cleanup requirements key archive
  [[ -d "$assets" && -d "${RUNNER_TEMP:-}" ]] || return 1
  [[ "${GITHUB_REPOSITORY_ID:-}" =~ ^[0-9]+$ ]] || return 1

  work=$(mktemp -d "$RUNNER_TEMP/clud-wheels.XXXXXX") || return 1
  printf -v cleanup '%q' "$work"
  trap "python -c 'import shutil, sys; shutil.rmtree(sys.argv[1], ignore_errors=True)' $cleanup" EXIT
  requirements="$work/requirements.txt"
  uv export --frozen --group dev --no-emit-project --format requirements-txt --no-header \
    > "$requirements" || return 1
  key=$(python -c '
import hashlib, platform, sys
from pathlib import Path

target = ":".join((sys.platform, platform.machine(), platform.python_version()))
print(hashlib.sha256(target.encode() + b"\0" + Path(sys.argv[1]).read_bytes()).hexdigest())
' "$requirements") || return 1
  archive="$assets/clud-uv-wheels-v1-$GITHUB_REPOSITORY_ID-$key.tar"
  uv venv --allow-existing --offline --python 3.11 .venv || return 1

  # Serialize rebuilding an absent or invalid archive. Flat files and an
  # updated mtime on each verified hit let the existing assets GC expire it.
  exec 9>>"$archive.lock" || return 1
  flock -w 150 9 || return 1
  touch -- "$archive.lock" || return 1
  if [[ -f "$archive" ]]; then
    if unpack_wheels "$archive" "$work/cached" &&
      install_wheels "$work/cached" "$requirements"; then
      touch -- "$archive" || true
      return 0
    fi
    echo "::warning::Rebuilding invalid local Python wheelhouse"
  fi

  mkdir -- "$work/download" || return 1
  timeout 140 python -m pip download --disable-pip-version-check --no-cache-dir \
    --require-hashes --only-binary=:all: --no-deps -r "$requirements" --dest "$work/download" ||
    return 1
  install_wheels "$work/download" "$requirements" || return 1
  publish_wheels "$work/download" "$archive" ||
    echo "::warning::Could not publish the verified Python wheelhouse"
)

if [[ "${CLUD_LOCAL_UV_CACHE:-}" == true && -n "${OBS_RUNNER_CACHE:-}" ]]; then
  if "${sync[@]}" --offline; then
    echo "Reused the slot-local uv package cache"
    exit 0
  fi
  if sync_from_wheelhouse "$OBS_RUNNER_CACHE/assets"; then
    echo "Installed locked packages from the shared local wheelhouse"
    exit 0
  fi
  echo "::warning::Local Python wheelhouse unavailable; using regular uv sync"
fi

"${sync[@]}"

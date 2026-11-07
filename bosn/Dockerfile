FROM rust:1.95.0-bookworm@sha256:6258907abe69656e41cd992e0b705cdcfabcbbe3db374f92ed2d47121282d4a1

# Bosn derives a managed image identity from this Dockerfile. Keep this
# project-specific label so an unrelated registry cannot claim the same tag.
LABEL org.opencontainers.image.title="clud-bosn-validation"

RUN apt-get update \
    # Debian package names. python-name-lint: allow-next-line
    && apt-get install -y --no-install-recommends clang lld python3 python3-pip python-is-python3 git pkg-config libssl-dev nodejs npm \
    && rm -rf /var/lib/apt/lists/* \
    && pip3 install --break-system-packages uv soldr \
    && npm install -g @anthropic-ai/claude-code@2.1.282 @openai/codex@0.164.0

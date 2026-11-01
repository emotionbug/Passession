# syntax=docker/dockerfile:1

# Every build runs for TARGETPLATFORM (native runner or QEMU); the build and
# runtime architecture must match. Neither stage needs Node or npm.
ARG RUST_IMAGE=rust:1.93.1-bookworm
FROM ${RUST_IMAGE} AS native-build

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends patchelf; \
    rm -rf /var/lib/apt/lists/*

WORKDIR /tmp/paseo-src
COPY . .

ARG PASEO_VERSION
ARG PASEO_BUILD_SOURCE_SHA
ARG PASEO_NATIVE_RELEASE=0
ARG PASEO_UPDATE_REPOSITORY
ARG PASEO_RELEASE_CHANNEL
# Compile and collect notices in this same stage: COPY . . retains workspace
# path dependencies and Cargo retains its fetched registry sources/config.
# Only the assembled native runtime and generated notices, never Cargo sources
# or build tools, cross into the final image. Plugins are installed separately.
RUN cargo xtask container-runtime /opt/paseo

FROM debian:bookworm-slim

ENV HOME=/home/paseo \
    PASEO_HOME=/home/paseo/.paseo \
    PASEO_LISTEN=0.0.0.0:6767 \
    PASEO_LOG_FORMAT=json \
    PASEO_LOG_LEVEL=info \
    CLAUDE_CONFIG_DIR=/home/paseo/.claude \
    CODEX_HOME=/home/paseo/.codex \
    XDG_CONFIG_HOME=/home/paseo/.config \
    XDG_DATA_HOME=/home/paseo/.local/share \
    XDG_STATE_HOME=/home/paseo/.local/state \
    XDG_CACHE_HOME=/home/paseo/.cache

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
      bash \
      ca-certificates \
      curl \
      git \
      gosu \
      openssh-client \
      procps \
      tini; \
    rm -rf /var/lib/apt/lists/*

COPY --from=native-build /opt/paseo /opt/paseo
RUN ln -s /opt/paseo/paseo /usr/local/bin/paseo

RUN set -eux; \
    existing_group="$(getent group 1000 | cut -d: -f1 || true)"; \
    if [ -n "$existing_group" ] && [ "$existing_group" != "paseo" ]; then \
      groupmod --new-name paseo "$existing_group"; \
    elif [ -z "$existing_group" ]; then \
      groupadd --gid 1000 paseo; \
    fi; \
    existing_user="$(getent passwd 1000 | cut -d: -f1 || true)"; \
    if [ -n "$existing_user" ] && [ "$existing_user" != "paseo" ]; then \
      usermod --login paseo --gid paseo --home /home/paseo --shell /bin/bash "$existing_user"; \
    elif [ -z "$existing_user" ]; then \
      useradd --uid 1000 --gid paseo --create-home --home-dir /home/paseo --shell /bin/bash paseo; \
    fi; \
    mkdir -p \
      /workspace \
      "$PASEO_HOME" \
      "$CLAUDE_CONFIG_DIR" \
      "$CODEX_HOME" \
      "$XDG_CONFIG_HOME" \
      "$XDG_DATA_HOME" \
      "$XDG_STATE_HOME" \
      "$XDG_CACHE_HOME"; \
    chown -R paseo:paseo /home/paseo /workspace

COPY docker/base/rootfs/ /
RUN chmod +x /usr/local/bin/paseo-docker-entrypoint /usr/local/bin/paseo-docker-healthcheck

WORKDIR /workspace

EXPOSE 6767
VOLUME ["/home/paseo"]

HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 \
  CMD ["/usr/local/bin/paseo-docker-healthcheck"]

ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/paseo-docker-entrypoint"]

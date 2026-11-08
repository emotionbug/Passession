# syntax=docker/dockerfile:1

# Every build runs for TARGETPLATFORM (native runner or QEMU); the build and
# runtime architecture must match. Neither stage needs Node or npm.
ARG RUST_IMAGE=rust:1.93.1-bookworm
FROM ${RUST_IMAGE} AS native-build

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends patchelf xz-utils; \
    rm -rf /var/lib/apt/lists/*

# libghostty-vt-sys builds the Ghostty VT library with Zig 0.15.2 and clones
# the pinned Ghostty source, so the build stage needs network access. Digests
# come from https://ziglang.org/download/index.json.
ARG ZIG_VERSION=0.15.2
RUN set -eux; \
    case "$(dpkg --print-architecture)" in \
      amd64) zig_arch=x86_64; zig_sha256=02aa270f183da276e5b5920b1dac44a63f1a49e55050ebde3aecc9eb82f93239 ;; \
      arm64) zig_arch=aarch64; zig_sha256=958ed7d1e00d0ea76590d27666efbf7a932281b3d7ba0c6b01b0ff26498f667f ;; \
      *) echo "unsupported Zig build architecture: $(dpkg --print-architecture)" >&2; exit 1 ;; \
    esac; \
    zig_dir="zig-${zig_arch}-linux-${ZIG_VERSION}"; \
    curl -fsSLo /tmp/zig.tar.xz "https://ziglang.org/download/${ZIG_VERSION}/${zig_dir}.tar.xz"; \
    echo "${zig_sha256}  /tmp/zig.tar.xz" | sha256sum -c -; \
    tar -xJf /tmp/zig.tar.xz -C /opt; \
    rm /tmp/zig.tar.xz; \
    ln -s "/opt/${zig_dir}/zig" /usr/local/bin/zig; \
    zig version

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

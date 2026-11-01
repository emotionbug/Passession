# Build from repository root: docker build -f packages/relay/Dockerfile .
FROM rust:1.93-bookworm AS build
WORKDIR /source
COPY Cargo.toml Cargo.lock ./
COPY packages ./packages
COPY xtask ./xtask
COPY plugin-examples ./plugin-examples
RUN cargo build --locked --release --package paseo-relay

FROM debian:bookworm-slim
RUN groupadd --gid 10001 relay && useradd --uid 10001 --gid relay --no-create-home relay
COPY --from=build /source/target/release/paseo-relay /usr/local/bin/paseo-relay
USER 10001:10001
ENV PASEO_RELAY_LISTEN=0.0.0.0:8080
EXPOSE 8080
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/paseo-relay"]

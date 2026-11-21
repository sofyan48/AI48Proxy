# syntax=docker/dockerfile:1
# Override with an audited tag@sha256:digest for reproducible release images.
ARG RUST_IMAGE=rust:bookworm
FROM ${RUST_IMAGE} AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --gid 10001 ai48proxy \
    && useradd --uid 10001 --gid 10001 --no-create-home --home-dir /var/lib/ai48proxy --shell /usr/sbin/nologin ai48proxy \
    && install -d -m 0700 -o 10001 -g 10001 /var/lib/ai48proxy
COPY --from=builder /build/target/release/ai48proxy /usr/local/bin/ai48proxy
COPY LICENSE THIRD_PARTY_NOTICES.md /usr/share/licenses/ai48proxy/
COPY licenses/ /usr/share/licenses/ai48proxy/licenses/
USER 10001:10001
WORKDIR /var/lib/ai48proxy
# Actual bind address and port are supplied by Home; this is only the default.
EXPOSE 8317
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/ai48proxy"]
CMD ["--identity-dir", "/var/lib/ai48proxy/identity"]

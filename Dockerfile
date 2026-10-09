# syntax=docker/dockerfile:1
#
# A Ferrix app as a `FROM scratch` image: one static binary (built by scripts/release.sh for x86_64 musl, with the
# web app embedded), CA certificates for outgoing TLS (S3, OAuth, SMTP, Let's Encrypt), and a data volume.
#
#   docker build -t todo-server .
#   docker run -p 8090:8090 -v todo-data:/data todo-server
#   docker run todo-server admin create you@example.com --password '…' --dir /data   # any CLI command
#
# For your own app: `docker build --build-arg PACKAGE=my-server --build-arg WEB=web -t my-server .` (WEB is the web
# app directory to build and embed, or "" for none).

ARG RUST_VERSION=1.99
ARG BUN_VERSION=1

FROM oven/bun:${BUN_VERSION} AS bun

FROM rust:${RUST_VERSION}-bookworm AS build
ARG PACKAGE=todo-server
ARG WEB=examples/todo/web
ARG ZIG_VERSION=0.15.2
ARG ZIGBUILD_VERSION=0.23.4
RUN apt-get update && apt-get install -y --no-install-recommends xz-utils file ca-certificates && rm -rf /var/lib/apt/lists/*
RUN curl -fsSL "https://ziglang.org/download/${ZIG_VERSION}/zig-x86_64-linux-${ZIG_VERSION}.tar.xz" | tar -xJ -C /opt \
    && ln -s "/opt/zig-x86_64-linux-${ZIG_VERSION}/zig" /usr/local/bin/zig
RUN cargo install --locked cargo-zigbuild --version "${ZIGBUILD_VERSION}"
COPY --from=bun /usr/local/bin/bun /usr/local/bin/bun
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    PACKAGE="${PACKAGE}" WEB="${WEB}" OUT=/out scripts/release.sh
# The data directory, owned by the unprivileged user the server runs as.
RUN mkdir -p /data && chown 65532:65532 /data

FROM scratch
ARG PACKAGE=todo-server
COPY --from=build /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=build /out/${PACKAGE} /app
COPY --from=build --chown=65532:65532 /data /data
ENV SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
USER 65532:65532
VOLUME /data
EXPOSE 8090
ENTRYPOINT ["/app"]
# Behind a proxy that terminates TLS. With --https, publish 80 and 443 instead (the user needs no extra rights: the
# ports inside the container are remapped, e.g. `-p 443:8443 -p 80:8080` with `--https-addr 0.0.0.0:8443
# --http-redirect 0.0.0.0:8080`).
CMD ["serve", "--http", "0.0.0.0:8090", "--dir", "/data"]

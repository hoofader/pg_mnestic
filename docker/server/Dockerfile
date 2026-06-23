# SPDX-License-Identifier: MIT
#
# The mnestic-server application image: the REST + MCP server (`serve`), the async worker
# (`worker`), and the operator CLIs. Built once and reused for every service in the compose
# stack. reqwest uses rustls, so the runtime needs no OpenSSL, only CA roots.
FROM rust:1.91 AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p mnestic-server --features serve,cli

FROM debian:bookworm-slim
# ca-certificates: outbound TLS to the providers. curl: the container healthcheck.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
  && rm -rf /var/lib/apt/lists/*
COPY --from=build \
  /src/target/release/serve \
  /src/target/release/worker \
  /src/target/release/issue-key \
  /src/target/release/list-keys \
  /src/target/release/revoke-key \
  /src/target/release/export-actor \
  /src/target/release/purge-actor \
  /usr/local/bin/
ENV MNESTIC_BIND=0.0.0.0:8080
EXPOSE 8080
# The process binds a network socket and needs no write access to the image, so drop root: a
# compromised server then has no privileges beyond running the binaries it shipped with.
RUN useradd --system --no-create-home --uid 10001 mnestic
USER mnestic
CMD ["serve"]

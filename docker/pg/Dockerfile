# SPDX-License-Identifier: AGPL-3.0-only
#
# Postgres 16 with pgvector (the base image) plus pg_graphwright, built from source via pgrx.
# The integration tests, CI, and a production deploy all run against this image so the database
# carries the extensions the engine expects. Multi-stage: the Rust/pgrx toolchain stays in the
# builder, and only the compiled extension artifacts land in the final image (a clean pgvector
# image), so the shipped image is small. pg_graphwright is pinned to a commit for a reproducible
# build; bump PG_GRAPHWRIGHT_REF to upgrade.

FROM pgvector/pgvector:pg16 AS build

ARG PG_GRAPHWRIGHT_REF=88e1844cdfa482080d2377e7bde4124cf0718c80
ARG CARGO_PGRX_VERSION=0.18.0

# clang/libclang are pgrx's bindgen requirement; postgresql-server-dev-16 supplies the headers
# and pg_config the extension is built against (the base image's own PG16).
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential clang libclang-dev pkg-config git curl ca-certificates \
      postgresql-server-dev-16 \
  && rm -rf /var/lib/apt/lists/*

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal \
  && rustc --version

RUN cargo install cargo-pgrx --version ${CARGO_PGRX_VERSION} --locked

# Point pgrx at the image's existing PG16 so it never downloads or builds its own Postgres.
RUN cargo pgrx init --pg16 /usr/bin/pg_config

# Build and install the extension into this stage's PG16 tree. --no-default-features drops the
# crate's default pg13 feature so it builds against pg16.
RUN git clone https://github.com/hoofader/pg_graphwright /tmp/pgw \
  && git -C /tmp/pgw checkout ${PG_GRAPHWRIGHT_REF} \
  && cd /tmp/pgw \
  && cargo pgrx install --release --no-default-features --features pg16 --pg-config /usr/bin/pg_config

# Final image: the clean pgvector base plus only the built extension artifacts (the shared
# object and its control/SQL), so none of the Rust build toolchain ships.
FROM pgvector/pgvector:pg16

COPY --from=build /usr/lib/postgresql/16/lib/pg_graphwright.so /usr/lib/postgresql/16/lib/
COPY --from=build /usr/share/postgresql/16/extension/pg_graphwright.control /usr/share/postgresql/16/extension/
COPY --from=build /usr/share/postgresql/16/extension/pg_graphwright--*.sql /usr/share/postgresql/16/extension/

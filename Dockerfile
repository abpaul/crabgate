FROM rust:1.98-bookworm AS dependencies
WORKDIR /build
RUN apt-get update && apt-get install -y --no-install-recommends libssl-dev pkg-config && rm -rf /var/lib/apt/lists/*
COPY Cargo.toml Cargo.lock ./
COPY migration/Cargo.toml ./migration/Cargo.toml
RUN mkdir -p src/bin migration/src \
    && touch src/lib.rs migration/src/lib.rs \
    && printf 'fn main() {}\n' > src/bin/main.rs \
    && cargo build --release --locked --bin crabgate-cli \
    && cargo clean --release --package crabgate --package migration \
    && rm -rf src migration/src

FROM dependencies AS build
COPY migration ./migration
COPY src ./src
COPY assets/static/404.html ./assets/static/404.html
COPY assets/static/shared/favicon.ico assets/static/shared/apple-touch-icon.png ./assets/static/shared/
COPY assets/static/marketing/tools/rates.json ./assets/static/marketing/tools/rates.json
RUN cargo build --release --locked --bin crabgate-cli \
    && objcopy --compress-debug-sections=zlib-gabi target/release/crabgate-cli

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl gosu libssl3 && rm -rf /var/lib/apt/lists/* && useradd --system --uid 10001 --home /app crabgate
WORKDIR /app
COPY --from=build /build/target/release/crabgate-cli /usr/local/bin/crabgate-cli
COPY assets ./assets
COPY config ./config
COPY scripts/docker-entrypoint.sh /usr/local/bin/docker-entrypoint.sh
COPY scripts/crabgate-writer-start.sh /usr/local/bin/crabgate-writer-start.sh
ARG GIT_SHA=""
ENV SENTRY_RELEASE=$GIT_SHA
EXPOSE 5150
ENTRYPOINT ["docker-entrypoint.sh"]
CMD ["crabgate-cli", "start", "--environment", "production"]

FROM rust:1.96.1-bookworm AS app-build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY migrations migrations
COPY crates crates
RUN --mount=type=cache,id=custody-app-cargo,target=/usr/local/cargo/registry \
    --mount=type=cache,id=custody-app-target,target=/src/target \
    cargo build --locked --release -p custody-web -p custody-signer -p evm-reader \
    && mkdir /out && cp target/release/custody-web target/release/custody-signer target/release/evm-reader /out/

FROM rust:1.88.0-bookworm AS mango-build
RUN apt-get update && apt-get install -y --no-install-recommends protobuf-compiler clang cmake && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY migrations migrations
COPY mango-worker/Cargo.toml mango-worker/Cargo.lock mango-worker/
COPY mango-worker/src mango-worker/src
ENV CARGO_NET_GIT_FETCH_WITH_CLI=true
RUN --mount=type=cache,id=custody-mango-cargo,target=/usr/local/cargo/registry \
    --mount=type=cache,id=custody-mango-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=custody-mango-target,target=/src/mango-worker/target \
    cargo build --locked --release --manifest-path mango-worker/Cargo.toml \
    && mkdir /out && cp mango-worker/target/release/mango-worker /out/

FROM debian:bookworm-slim AS runtime
COPY LICENSE /usr/share/doc/multichain-custody-demo/LICENSE
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates libssl3 && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --create-home custody \
    && install -d -m 0700 -o custody -g custody /data/keystore /data/credentials
WORKDIR /app
USER custody

FROM runtime AS app
COPY --from=app-build /out/ /usr/local/bin/
COPY config config
CMD ["custody-web"]

FROM runtime AS mango
COPY config/mango-testnet-coins.json config/mango-testnet-coins.json
COPY --from=mango-build /out/mango-worker /usr/local/bin/
CMD ["mango-worker", "--follow"]

FROM app AS signing
COPY --from=mango-build /out/mango-worker /usr/local/bin/
CMD ["custody-signer","serve"]

# PushIt controller — WASM runtime in-process, no child processes/sockets needed.
# Builder = full rust image (buildpack-deps): aws-lc-sys needs cmake+perl,
# which rust:1-slim does not ship.
FROM rust:1 AS build
WORKDIR /src
# manifest layer first for dep caching
COPY Cargo.toml Cargo.lock ./
COPY controller ./controller
# workspace member — required for manifest resolution even though only the
# controller is built
COPY cli ./cli
RUN cargo build --release -p pushit-controller

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/pushit-controller /usr/local/bin/pushit-controller
# non-root; /data is the only writable path (SQLite, apps, logs, appdata)
RUN useradd --system --home /data pushit && mkdir -p /data && chown pushit:pushit /data
USER pushit
VOLUME /data
ENV PUSHIT_DATA=/data
EXPOSE 9100
ENTRYPOINT ["pushit-controller"]

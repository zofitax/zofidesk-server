# syntax=docker/dockerfile:1
# ZofiDesk server image: hbbs with the API and the embedded relay, plus zofidesk-admin.
# Build from the repository root: docker build -f deploy/Dockerfile .

FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked --bin hbbs --bin zofidesk-admin --bin rustdesk-utils \
    && mkdir /out \
    && cp target/release/hbbs target/release/zofidesk-admin target/release/rustdesk-utils /out/

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --home-dir /data --shell /usr/sbin/nologin zofidesk \
    && mkdir /data \
    && chown zofidesk:zofidesk /data
COPY --from=build /out/ /usr/local/bin/

# hbbs keeps its key (id_ed25519), db_v2.sqlite3 and zofidesk.sqlite3 in the working directory.
WORKDIR /data
VOLUME /data
USER zofidesk

# The relay runs inside hbbs so it can enforce MUST_LOGIN; there is no separate hbbr.
ENV EMBEDDED_RELAY=Y

# 21114 API (keep it behind the HTTPS proxy), 21115 NAT test, 21116 rendezvous,
# 21117 relay, 21118/21119 websocket rendezvous/relay.
EXPOSE 21114 21115 21116 21116/udp 21117 21118 21119

CMD ["hbbs"]

# Server + web console, and the self-hosted iroh-relay.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p irmm-server --bin irmm-server -p irmm-release --bin irmm-release

FROM node:22-bookworm AS web
WORKDIR /web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web/ ./
RUN npm run build

FROM rust:1-bookworm AS relay-build
RUN cargo install iroh-relay --version 1.3.0 --features server --locked

FROM debian:bookworm-slim AS server
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/irmm-server /src/target/release/irmm-release /usr/local/bin/
COPY --from=web /web/dist /app/web
ENV IRMM_STATIC_DIR=/app/web IRMM_DATA_DIR=/data IRMM_HTTP_ADDR=0.0.0.0:8080 IRMM_IROH_ADDR=0.0.0.0:7843
VOLUME /data
EXPOSE 8080 7843/udp
CMD ["irmm-server"]

FROM debian:bookworm-slim AS relay
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=relay-build /usr/local/cargo/bin/iroh-relay /usr/local/bin/
COPY deploy/relay-entrypoint.sh /usr/local/bin/
VOLUME /certs
EXPOSE 80 443 7842/udp
ENTRYPOINT ["sh", "/usr/local/bin/relay-entrypoint.sh"]

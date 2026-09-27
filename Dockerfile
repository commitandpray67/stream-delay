# Headless stream-delay (streamdelayd) for servers and two-PC setups.
#
#   docker run -d --name stream-delay -p 1935:1935 -p 127.0.0.1:7788:7788 \
#     -v stream-delay:/data ghcr.io/commitandpray67/stream-delay
#   docker exec stream-delay streamdelayd urls
#
# Then stream from OBS to rtmp://<host>:1935/live with the ingest key, and open
# the dashboard link `streamdelayd urls` shows (they are not written to the logs).
# The ingest key is generated and saved in /data; to choose one, set
# STREAMDELAY_INGEST_KEY to at least 16 characters (not a pattern such as aaaa… or abcd…).
# Port 7788 (dashboard and API, plain HTTP) stays on this machine; see the user
# guide before publishing it more widely.

# Base images are pinned by digest; Renovate proposes new ones.
FROM node:22-bookworm-slim@sha256:43ac6c60b8f89723f746e8a92ce91abd5017e627ce1ddfe4238355d3a30b772c AS ui
RUN corepack enable
WORKDIR /src/ui
COPY ui/package.json ui/pnpm-lock.yaml ui/pnpm-workspace.yaml ./
RUN pnpm install --frozen-lockfile
COPY ui/ ./
RUN pnpm build

FROM rust:1.98.1-bookworm@sha256:93ce27a88655056a51dbdd8f5f2d7ddc071c7b0070fb288a37b5a285fc83971e AS build
WORKDIR /src
COPY . .
COPY --from=ui /src/ui/dist ui/dist
RUN cargo build --release -p streamdelayd && strip target/release/streamdelayd

FROM debian:bookworm-slim@sha256:3783cc01769c7b2b1b83a5c5ad96c815348e28ed7da68e2e3687004faa906251
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --home /data --create-home streamdelay
COPY --from=build /src/target/release/streamdelayd /usr/local/bin/streamdelayd
USER streamdelay
ENV STREAMDELAY_CONFIG=/data/config.toml
VOLUME /data
EXPOSE 1935 7788
# `docker ps` shows whether it answers. If you change --api, change this too
# (docker run --health-cmd).
HEALTHCHECK --interval=30s --timeout=10s --start-period=10s --retries=3 \
    CMD ["streamdelayd", "health", "--url", "http://127.0.0.1:7788"]
ENTRYPOINT ["streamdelayd", "run", "--no-keychain", "--allow-lan", "--ingest", "0.0.0.0:1935", "--api", "0.0.0.0:7788"]

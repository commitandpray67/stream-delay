# Headless stream-delay (streamdelayd) for servers and two-PC setups.
#
#   docker run -d --name stream-delay -p 1935:1935 -p 127.0.0.1:7788:7788 \
#     -v stream-delay:/data ghcr.io/commitandpray67/stream-delay
#   docker exec stream-delay streamdelayd urls
#
# Then stream from OBS to rtmp://<host>:1935/live with the ingest key, and open
# the dashboard link `streamdelayd urls` shows (they are not written to the logs).
# The ingest key is generated and saved in /data; to choose one, set
# STREAMDELAY_INGEST_KEY to at least 16 characters.
# Port 7788 (dashboard and API, plain HTTP) stays on this machine; see the user
# guide before publishing it more widely.

FROM node:22-bookworm-slim AS ui
RUN corepack enable
WORKDIR /src/ui
COPY ui/package.json ui/pnpm-lock.yaml ui/pnpm-workspace.yaml ./
RUN pnpm install --frozen-lockfile
COPY ui/ ./
RUN pnpm build

FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
COPY --from=ui /src/ui/dist ui/dist
RUN cargo build --release -p streamdelayd && strip target/release/streamdelayd

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --home /data --create-home streamdelay
COPY --from=build /src/target/release/streamdelayd /usr/local/bin/streamdelayd
USER streamdelay
ENV STREAMDELAY_CONFIG=/data/config.toml
VOLUME /data
EXPOSE 1935 7788
ENTRYPOINT ["streamdelayd", "run", "--no-keychain", "--allow-lan", "--ingest", "0.0.0.0:1935", "--api", "0.0.0.0:7788"]

# Headless stream-delay (streamdelayd) for servers and two-PC setups.
#
#   docker run -p 1935:1935 -p 7788:7788 -v stream-delay:/data \
#     -e STREAMDELAY_INGEST_KEY=choose-a-secret ghcr.io/commitandpray67/stream-delay
#
# Then stream from OBS to rtmp://<host>:1935/live with the ingest key, and open
# the dashboard link printed in the logs (`docker logs`).

FROM node:22-bookworm-slim AS ui
RUN corepack enable
WORKDIR /src/ui
COPY ui/package.json ui/pnpm-lock.yaml ./
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

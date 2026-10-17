# skim: build with the pinned toolchain, run on a slim base.
#   docker build -t skim .
#   docker run --rm -p 8545:8545 -p 9130:9130 -p 9130:9130/udp -p 9131:9131/udp \
#     -p 30388:30388 -p 30388:30388/udp -v skim-data:/data \
#     skim run --rpc 0.0.0.0:8545 --data-dir /data
FROM rust:1.97.0-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p skim-cli && cp target/release/skim /skim

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && apt-get clean && find /var/lib/apt/lists -mindepth 1 -delete \
    && useradd --system --home /data skim && mkdir -p /data && chown skim /data
COPY --from=build /skim /usr/local/bin/skim
USER skim
VOLUME /data
# JSON-RPC, consensus (TCP + UDP, QUIC), execution (TCP + UDP), metrics
EXPOSE 8545 9130 9130/udp 9131/udp 30388 30388/udp 9465
ENTRYPOINT ["skim"]
CMD ["run", "--rpc", "0.0.0.0:8545", "--data-dir", "/data"]

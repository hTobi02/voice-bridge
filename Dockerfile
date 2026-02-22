FROM rust:1.88-slim-trixie AS build
WORKDIR /build

RUN apt-get update && apt-get install -y --no-install-recommends \
    ffmpeg pkg-config libopus-dev libssl-dev build-essential \
  && rm -rf /var/lib/apt/lists/*

COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release
# Binary at: /build/target/release/voice_bridge


FROM debian:trixie-slim AS final
WORKDIR /app
RUN apt update && \
    apt install -y ffmpeg
COPY --from=build /build/target/release/voice_bridge .
COPY entrypoint.sh /
ENTRYPOINT [ "/entrypoint.sh" ]
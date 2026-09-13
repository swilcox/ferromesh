# syntax=docker/dockerfile:1
FROM rust:1.94-trixie AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p ferromeshd \
 && install -D target/release/ferromeshd /out/ferromeshd

FROM debian:trixie-slim
COPY --from=build /out/ferromeshd /usr/local/bin/ferromeshd
VOLUME ["/data"]
ENTRYPOINT ["ferromeshd"]
CMD ["serve", "--config", "/config/ferromesh.toml"]

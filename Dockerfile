# syntax=docker/dockerfile:1

FROM rust:1.98.1-alpine AS builder
RUN apk add --no-cache musl-dev

WORKDIR /app

# Build dependencies separately so they are cached across source-only changes.
COPY Cargo.toml Cargo.lock ./
RUN mkdir -p src \
    && echo "fn main() {}" > src/main.rs \
    && echo "" > src/lib.rs \
    && cargo build --release \
    && rm -rf src

COPY src ./src
RUN touch src/main.rs src/lib.rs \
    && cargo build --release

FROM gcr.io/distroless/static-debian12:nonroot

COPY --from=builder /app/target/release/iot-ftp-upload-gateway /usr/local/bin/iot-ftp-upload-gateway

# Documentation only; actual publishing is done via `docker run -p` / ECS task definition
# port mappings. The PASV range must match GATEWAY_PASSIVE_PORT_RANGE_START/_END at runtime.
EXPOSE 21
EXPOSE 10000-20000

ENTRYPOINT ["/usr/local/bin/iot-ftp-upload-gateway"]

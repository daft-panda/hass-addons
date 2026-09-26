# syntax=docker/dockerfile:1

ARG BUILD_FROM=ghcr.io/home-assistant/base:3.24

# Build stage - compile Rust binary using the same base image
FROM $BUILD_FROM AS builder

# Install current stable Rust via rustup rather than Alpine's cargo package,
# since the apk toolchain lags behind crate MSRV requirements.
RUN apk add --no-cache curl ca-certificates cmake make g++ gcc musl-dev perl

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --default-toolchain stable --profile minimal --no-modify-path \
    && rustc --version

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --locked --release

# Runtime stage - minimal image with only the binary
FROM $BUILD_FROM

COPY --from=builder /build/target/release/ebus-thermostat /usr/local/bin/
COPY run.sh /

RUN chmod a+x /run.sh

LABEL \
    org.opencontainers.image.title="ebus thermostat" \
    org.opencontainers.image.description="Control your ebus heater connected via ebusd" \
    org.opencontainers.image.source="https://github.com/daft-panda/hass-addons" \
    org.opencontainers.image.licenses="Apache License 2.0"

# Running from a script instead executing the binary directly is needed as env vars do not get injected otherwise
CMD [ "/run.sh" ]

# syntax=docker/dockerfile:1

ARG BUILD_FROM

# Build stage - compile Rust binary using the same base image
FROM $BUILD_FROM AS builder

RUN apk add --no-cache cargo cmake make g++ perl

WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src

RUN cargo build --locked --release

# Runtime stage - minimal image with only the binary
FROM $BUILD_FROM

COPY --from=builder /build/target/release/ebus-thermostat /usr/local/bin/
COPY run.sh /

RUN chmod a+x /run.sh

# Running from a script instead executing the binary directly is needed as env vars do not get injected otherwise
CMD [ "/run.sh" ]

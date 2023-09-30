# syntax=docker/dockerfile:1

ARG RUST_VERSION=1.72.1
ARG APP_NAME=ebusd-thermostat
FROM rust:${RUST_VERSION}-slim-bullseye AS build
ARG APP_NAME
WORKDIR /

RUN \
    <<EOF
set -e
cargo build --locked --release
cp ./target/release/$APP_NAME /bin/ebusd-thermostat
EOF

ARG BUILD_FROM
FROM $BUILD_FROM

COPY --from=build /bin/ebusd-thermostat /bin/

COPY rootfs /

CMD [ "/bin/ebusd-thermostat" ]
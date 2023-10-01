# syntax=docker/dockerfile:1
ARG BUILD_FROM
FROM $BUILD_FROM

COPY ./ ./

RUN set -e && \
    apk add --no-cache cargo && \
    cargo build --locked --release

RUN chmod a+x ./run.sh

# Running from a script instead executing the binary directly is needed as env vars do not get injected otherwise
CMD [ "./run.sh" ]
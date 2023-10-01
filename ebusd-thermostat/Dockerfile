# syntax=docker/dockerfile:1
ARG BUILD_FROM
FROM $BUILD_FROM

COPY ./ ./

RUN set -e && \
    apk add --no-cache cargo && \
    cargo build --locked --release

RUN chmod a+x ./run.sh

CMD [ "./run.sh" ]
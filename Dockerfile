FROM rust:1.92.0-alpine3.23 AS builder

LABEL maintainer="hello@rustfs.com"

WORKDIR /app

COPY . .

RUN cargo build --release

FROM alpine:3.23

COPY --from=builder /app/target/release/rc /usr/bin/rc
COPY --from=builder /app/LICENSE-* /licenses/

ENTRYPOINT ["rc"]

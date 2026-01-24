FROM rust:1.92.0-alpine3.23 AS builder

LABEL maintainer="hello@rustfs.com"

WORKDIR /app

# Install build dependencies for Alpine
RUN apk add --no-cache musl-dev openssl-dev openssl-libs-static

COPY . .

RUN cargo build --release

FROM alpine:3.23

# Install CA certificates for HTTPS
RUN apk add --no-cache ca-certificates

COPY --from=builder /app/target/release/rc /usr/bin/rc
COPY --from=builder /app/LICENSE-* /licenses/

ENTRYPOINT ["rc"]

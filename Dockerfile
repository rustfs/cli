FROM rust:1.92.0-alpine3.23 AS builder

LABEL maintainer="hello@rustfs.com"

WORKDIR /app

# Install build dependencies for Alpine
RUN apk add --no-cache musl-dev openssl-dev openssl-libs-static

COPY . .

RUN cargo build --release

FROM alpine:3.23

# Install runtime dependencies:
# - ca-certificates: HTTPS support
# - jq/yq: lightweight JSON/YAML processing for container workflows
RUN apk add --no-cache ca-certificates jq yq

COPY --from=builder /app/target/release/rc /usr/bin/rc
COPY --from=builder /app/LICENSE-* /licenses/

ENTRYPOINT ["rc"]

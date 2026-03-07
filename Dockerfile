# Build Stage
FROM rust:1.93-alpine AS builder
WORKDIR /usr/src/
# Install required build dependencies
RUN apk add --no-cache musl-dev pkgconfig openssl-dev openssl-libs-static gcc g++ make

# - Install dependencies
WORKDIR /usr/src
RUN USER=root cargo new jobfather
WORKDIR /usr/src/jobfather
COPY Cargo.toml Cargo.lock ./
RUN cargo build --release

# - Copy source
COPY src ./src
RUN touch src/main.rs && cargo build --release

# ---- Runtime Stage ----
FROM alpine:latest AS runtime
COPY --from=builder /usr/src/jobfather/target/release/jobfather /usr/local/bin/jobfather
USER 1000
CMD ["jobfather"]

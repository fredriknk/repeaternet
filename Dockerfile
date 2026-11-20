# syntax=docker/dockerfile:1
FROM rust:1.100-alpine AS builder
ARG TARGETARCH
WORKDIR /build/rust
COPY rust/ ./
# The Alpine host compiler already targets musl.
ENV CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=cc
# Refresh local source timestamps when layering a new checkout over the target cache.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/rust/target,sharing=locked \
    test "$TARGETARCH" = amd64 \
    && find crates -type f -exec touch {} + \
    && cargo build --locked --release --target x86_64-unknown-linux-musl -p repeaternet \
    && install -D -m 755 target/x86_64-unknown-linux-musl/release/repeaternet /out/repeaternet \
    && mkdir /data && chown 1000:1000 /data

FROM scratch
COPY --from=builder /out/repeaternet /repeaternet
COPY --from=builder /etc/ssl/certs/ca-certificates.crt /etc/ssl/certs/ca-certificates.crt
COPY --from=builder --chown=1000:1000 /data /data
USER 1000:1000
ENV RF_PLANNER_DATA=/data \
    TMPDIR=/data \
    SSL_CERT_FILE=/etc/ssl/certs/ca-certificates.crt
EXPOSE 8000
ENTRYPOINT ["/repeaternet", "--host", "0.0.0.0", "--port", "8000", "--no-browser"]

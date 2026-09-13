FROM rust:1.90-bookworm AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/vllm-reconciler /usr/local/bin/vllm-reconciler
USER 65532:65532
EXPOSE 9090
HEALTHCHECK --interval=30s --timeout=3s CMD curl -fsS http://127.0.0.1:9090/healthz || exit 1
ENTRYPOINT ["vllm-reconciler"]
CMD ["--config", "/etc/reconciler/config.yaml"]

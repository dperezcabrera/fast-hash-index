# Construcción
FROM rust:1.80-slim AS builder
WORKDIR /app

# Pre-cachar dependencias
COPY Cargo.toml Cargo.lock* ./

# Copia del código real
COPY src ./src
RUN cargo build --release

# Imagen mínima de runtime con el binario
FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/fast-hash-index /usr/local/bin/fast-hash-index
ENTRYPOINT ["fast-hash-index"]


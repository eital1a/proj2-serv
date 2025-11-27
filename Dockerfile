# Stage 1: build with Rust toolchain
FROM rust:1.71 AS builder
WORKDIR /app
# copy source and build
COPY . .
RUN cargo build --release

# Stage 2: small runtime image
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/proj2-serv /usr/local/bin/proj2-serv
# Informational exposes (runtime mapping done with docker run)
EXPOSE 8080
EXPOSE 7070/udp
# Run as non-root user for safety
RUN useradd -m appuser
USER appuser
CMD ["/usr/local/bin/proj2-serv"]

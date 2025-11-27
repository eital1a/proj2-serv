# Stage 1 build using a recent Rust toolchain
FROM rust:1.91 as builder
WORKDIR /app
COPY . .
RUN cargo build --release

# Stage 2 runtime
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/proj2-serv /usr/local/bin/proj2-serv
EXPOSE 8080
EXPOSE 7070/udp
USER 1000
CMD ["/usr/local/bin/proj2-serv"]

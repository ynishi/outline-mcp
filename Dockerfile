# syntax=docker/dockerfile:1.7

FROM rust:1.88-slim-bookworm AS builder
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY docs ./docs
RUN cargo build --release --locked

FROM debian:bookworm-slim
LABEL io.modelcontextprotocol.server.name="io.github.ynishi/outline-mcp"
LABEL org.opencontainers.image.source="https://github.com/ynishi/outline-mcp"
LABEL org.opencontainers.image.licenses="MIT OR Apache-2.0"
LABEL org.opencontainers.image.description="Tree-structured knowledge base as an MCP server"

COPY --from=builder /build/target/release/outline-mcp /usr/local/bin/outline-mcp

WORKDIR /data
ENTRYPOINT ["outline-mcp"]

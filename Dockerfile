# Build stage - uses statically linked bun-webkit
FROM rust:1.92 AS builder

WORKDIR /app
COPY . .

# Build release binary (bun-webkit downloaded automatically)
RUN cargo build --release -p otter-cli

# Runtime stage - must match builder's glibc version (2.39+)
FROM debian:trixie-slim

# Install minimal runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy binary (statically linked JSC)
COPY --from=builder /app/target/release/otter /usr/local/bin/

# Create non-root user
RUN useradd -m otter
USER otter
WORKDIR /home/otter

ENTRYPOINT ["otter"]
CMD ["--help"]

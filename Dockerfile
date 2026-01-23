# Build stage
FROM rust:1.92 AS builder

WORKDIR /app
COPY . .

# Build release binary
RUN cargo build --release -p otterjs

# Runtime stage
FROM debian:trixie-slim

# Install minimal runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Copy binary
COPY --from=builder /app/target/release/otter /usr/local/bin/

# Create non-root user
RUN useradd -m otter
USER otter
WORKDIR /home/otter

ENTRYPOINT ["otter"]
CMD ["--help"]

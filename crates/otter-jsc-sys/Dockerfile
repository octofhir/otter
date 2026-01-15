FROM rust:1.92-bookworm

RUN apt-get update && apt-get install -y \
    libjavascriptcoregtk-4.1-dev \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

COPY Cargo.toml build.rs ./
COPY src ./src

RUN cargo build --release

CMD ["cargo", "test"]

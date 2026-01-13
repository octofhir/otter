set shell := ["/bin/sh", "-cu"]

fmt:
    cargo fmt --all

lint:
    cargo clippy --all-targets --all-features -- -D warnings

test:
    cargo test --all --all-features

build:
    cargo build --all-targets

run *args:
    cargo run -p otter-cli -- {{args}}

clean:
    cargo clean

# Operations

## Setup
```bash
# Install Rust toolchain (if not already installed)
rustup install stable
rustup default stable
```

## Build
```bash
cargo build                # debug build
cargo build --release      # optimized release build
```

## Test & Verify
```bash
cargo test                 # run unit tests
cargo check                # quick compile check (no binary)
cargo run -- stress        # run stress test suite (default params)
cargo run -- stress --runs 100 --size 5000   # extended stress: 100 runs, 5KB files
cargo run -- stress --runs 20 --size 50000   # large-file stress: 20 runs, 50KB files
```

## Recovery CLI
```bash
cargo run -- corrupt   input.zip -o corrupted.zip    # apply \n→\r\n corruption
cargo run -- recover   corrupted.zip -o recovered.zip # recover archive
```

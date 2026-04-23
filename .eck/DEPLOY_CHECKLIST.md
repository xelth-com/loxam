# Deploy Checklist

- [ ] Run `cargo test` — all tests pass
- [ ] Run `cargo run -- stress` — stress suite passes with 0 failures
- [ ] Build release binary: `cargo build --release`
- [ ] Verify binary exists: `target/release/loxam.exe` (Windows) or `target/release/loxam` (Linux/macOS)
- [ ] Confirm no new compiler warnings: `cargo check 2>&1`
- [ ] Test release binary end-to-end: `./target/release/loxam recover <file>`
- [ ] Tag release commit (if applicable)

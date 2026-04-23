# Loxam Context

Project: loxam | Type: rust binary | Stack: Rust + clap + flate2 + crc32fast + rand + anyhow

## Purpose
Recovers ZIP archives corrupted by text-mode transfer (`\n` → `\r\n` line-ending conversion applied to binary data).

## Architecture
- `main.rs` — CLI entry point (clap): `test`, `stress`, `corrupt`, `recover`
- `corrupt.rs` — Applies `\n` → `\r\n` corruption to binary data (preserves existing `\r\n`)
- `recover.rs` — Recovery engine: remove-all baseline, per-file CRC32 fix, Deflate DFS backtracking
- `zip_util.rs` — Minimal ZIP writer, parser, CRC32/deflate validation

## Key Constraints
- ZIP local file headers use deflate (method 8), PK signatures preserved (no 0x0A/0x0D bytes)
- Natural `\r\n` in compressed binary data (~1 per 64KB) must be kept; inserted `\r` (~1 per 256B) must be removed
- Target scale: 380MB archives with ~6000 natural CRLFs and ~1.5M inserted CRs

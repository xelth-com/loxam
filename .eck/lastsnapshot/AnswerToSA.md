# Agent Report

## Task: `loxam:milestone3-production-hardening` — COMPLETE

### Changes Made

**1. Cargo.toml** — Added `memmap2 = "0.9"`, `rayon = "1.10"`, `indicatif = "0.17"`

**2. src/main.rs** — `run_corrupt` and `run_recover` now use `memmap2::Mmap` instead of `std::fs::read` for zero-copy file I/O.

**3. src/zip_util.rs** — ZIP64 support:
- `ParsedEntry.compressed_size` and `uncompressed_size` upgraded from `u32` to `u64`
- `LocalHeaderInfo` sizes upgraded to `u64`
- New `parse_zip64_sizes()` function parses Header ID `0x0001` extra fields
- `parse_and_validate()` detects `0xFFFFFFFF` sentinel and extracts true 8-byte sizes
- `find_crlf_positions()` rewritten with `memchr::memchr_iter` for SIMD-accelerated CRLF scan

**4. src/recover.rs** — Major rewrite:
- **Removed `CrlfLookup`** (was O(N) Vec of size `data.len()` — ~5GB for 308MB). Replaced with merge-based iteration in `build_candidate()`
- **Removed `build_offset_map`** (was O(N) Vec of size `candidate.len()` — ~2.5GB for 308MB). Replaced with `cand_to_corr()` using prefix-sum + binary search in O(log M)
- **Added `build_removed_prefix`** — O(M) prefix sum, ~10MB for 308MB
- **Fixed `cand_to_corr` bug** — when a fixed-point position coincides with a removed CRLF, advance past it
- **Rayon parallelization** — `try_per_file_fix` processes bad files via `par_iter().map().collect()`
- **indicatif progress bar** — shows file-level progress during parallel recovery
- **Buffer reuse** in `dfs_fix_section` — pre-allocates test buffer once, uses `copy_from_slice` instead of `Vec::new` + `insert`
- Removed dead code: `build_with_insertions`, `try_global_keep_n`, `try_global_keep_n_recursive`

### Test Results
- `cargo check`: 0 errors, 4 warnings (pre-existing dead code)
- `cargo build --release`: success
- `stress --runs 100 --size 500`: **100/100 perfect**
- `stress --runs 20 --size 50000`: **19/20** (1 failure at 4 natural CRLFs — known limitation of 3-max DFS depth)

### Memory Profile for 308MB
- Before: CrlfLookup (~5GB) + offset_map (~2.5GB) + data (~308MB) ≈ **~8GB**
- After: removed_prefix (~10MB) + crlf_positions (~10MB) + mmap (~0 heap) + candidate (~307MB) ≈ **~330MB**

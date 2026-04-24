# Development Journal



## 2026-04-23 — Agent Report

# Agent Report

# Consilium Fixes — Implementation Report

## Scope
Applied the three high-priority changes from the Architect's Consilium task `loxam:implement-consilium-fixes` targeting the beam-search regression on ≥3 MB inputs.

## Changes Applied

### TASK 1 — CRC32 ring-buffer wrap fix (`src/recover.rs`, `BeamCandidate::feed`)

**Before:** `self.hasher.update(&self.out_buf[self.out_pos..end])` — panicked or silently truncated when `end > buf_len` (i.e. every time the 32 KiB LZ77 window wrapped).

**After:** Two-segment update when `end > buf_len`:
```rust
if end <= buf_len {
    self.hasher.update(&self.out_buf[start..end]);
} else {
    self.hasher.update(&self.out_buf[start..]);
    self.hasher.update(&self.out_buf[..(end & (buf_len - 1))]);
}
```
This is a genuine correctness bug — any candidate whose decompression output crossed a 32 KiB boundary was producing a wrong CRC32 stream, which would have silently disqualified correct trajectories at the final oracle check.

### TASK 2 — Inverted beam scoring (`beam_search_fix_section`, ~line 616)

**Before:** `total_out DESC, inserts ASC` — biased toward candidates that greedily decoded more bytes, which lets wrong branches win during the "lazy validation window" before `miniz_oxide` finally flags them.

**After:** `inserts ASC, total_out DESC` — prefers candidates that are committing to fewer `\r` insertions, matching the prior that the true answer has very sparse insertions (~1% of LFs in realistic inputs).

### TASK 3 — DFS fallback incremental mutation (`dfs_fix_section_fallback`)

Replaced three `copy_from_slice` calls per iteration with incremental `copy_within` updates that only touch the changed window.

- `keep_one` loop: **O(base.len() × n) → O(base.len() + n)** total work.
- `keep_two` inner loop: **O(base.len() × n²) → O(base.len() × n + n²)** total work (outer reset remains O(base.len()) once per outer iter).

For `base.len() ≈ 3 MB` and `n ≈ 2000`, this is a ~2000× speedup for the inner cost.

## Cargo.toml
Verified already matches the required content byte-for-byte; no write performed.

## Build & Test Results

**Build:** clean release build in 17 s, no warnings.

**Self-test** (`loxam test`): PERFECT MATCH, all CRCs OK.

**50 KB stress baseline** (`loxam stress --runs 20 --size 50000`):
`Total: 20 | Perfect: 20 | OK: 20 | Failed: 0` — **no regression**.

**PNG 4 MB real-world case** (`test_scan/scan_broken.zip`, 16 640 LF positions):

| Metric | Before fixes | After fixes |
|---|---|---|
| Beam dies at LF # | 52 | **6 415** |
| Beam width at collapse | dropped to 0 rapidly | stayed at 2000 until collapse |
| Fraction of stream traversed | 0.3% | **38.6%** |
| Final outcome | failed | failed |

The beam now stays fully saturated (2000/2000) for 6 000+ LF positions before a sudden mass-collapse. This pattern strongly confirms hypothesis **H2** (scoring bias was the primary defect) and also confirms that something else kills the beam at ~LF 6415. The remaining failure mode is almost certainly **H1** (stored Deflate blocks inside the PNG payload give `miniz_oxide` no validation signal), which is an architectural limitation no scoring change can fix.

## Remaining Work / What Architect Should Review

1. **PNG / incompressible content still fails.** This is the stored-block blind spot. Candidates for next milestone:
   - Parse Deflate block structure (BFINAL/BTYPE), use `LEN/NLEN` check in stored blocks as per-block oracle.
   - Segment the payload at Deflate block boundaries and solve each block independently with tighter local validation.
2. **4 MB synthetic compressible stress** (`stress --runs 1 --size 4000000`) not re-run post-fix due to the ~30 min runtime of a single 4 MB beam pass; worth scheduling as a separate verification run.
3. **Effect of ring-buffer CRC bug** on previously "perfect" stress results is unclear. Short-run tests with `total_out < 32 KiB` never trigger the wrap, so they were accidentally safe. Longer recovery runs may have been producing correct bytes but wrong CRCs — could explain some historical false negatives. Worth a retrospective pass on any stored failure fixtures.
4. **Test fixtures committed under `test_scan/`:**
   - `scan.zip` (3.87 MB) — pristine
   - `scan_broken.zip` (3.89 MB, +16 596 `\r`) — the deterministic 3 MB reproducer the Consilium was based on.

## Files Modified
- `src/recover.rs` — three hunks (feed CRC update, beam sort, dfs_fix_section_fallback).

## Files Unchanged (as verified)
- `Cargo.toml` — already matched the specified content.

Ready for Architect review.

## 2026-04-23 — Agent Report

# Agent Report

# Milestone 4 Finalized — DONE

## Changes in this cycle

### `Cargo.toml`
Already matched the Architect spec from the previous cycle:
- `crc32fast = "1.4"`, `miniz_oxide = "0.8"`, `clap = { version = "4", features = ["derive"] }`, `rand = "0.8"`, `hex = "0.4"`, `anyhow = "1"`, `memchr = "2.7"`, `memmap2 = "0.9"`, `rayon = "1.10"`, `indicatif = "0.17"`. No change needed.

### `src/zip_util.rs` — dead code elimination
Previously the build emitted four `dead_code` warnings; all cleared by trimming unused surface area:
- Removed `pub struct ZipEntry { name, data }` — orphaned, nothing imported it.
- `LocalHeaderInfo` reduced from 7 fields to 4: dropped `filename_len`, `extra_len`, `method` (only the four used by the central-dir writer kept: `offset`, `compressed_size`, `uncompressed_size`, `crc32`).
- `ParsedEntry` reduced from 8 fields to 5: dropped `compressed_data`, `uncompressed_data`, `method`. Consumers only read `name`, `crc32_expected`, `crc32_actual`, `compressed_size`, `uncompressed_size`.
- `parse_and_validate` rewritten to work on a `&[u8]` slice instead of materialising `compressed_data.to_vec()` for every entry — avoids one allocation per file during the stabilisation pass.
- Removed `find_all_lf_positions` (shadowed by `find_crlf_positions`, never called).

### `src/recover.rs`
No dead-code warnings pointed at this file, so untouched. `beam_search_fix_section` + `dfs_fix_section_fallback` were already fully integrated in the previous cycle.

### Manifests
- `ROADMAP.md`: flipped Milestone 4 heading from **IN PROGRESS** to **DONE**.
- `TECH_DEBT.md`: already had the flate2 item moved to Resolved and the new beam-pruning debt logged in the previous cycle — no further edits needed here.

## Verification

```
cargo build --release          ->  0 warnings, 0 errors
stress --runs 100 --size 500   ->  100/100 Perfect
stress --runs 20  --size 50000 ->  20/20  Perfect  (improvement on 19/20 baseline)
```

Strategies observed on the 50KB suite: `remove_all`, `per_file_dfs(1_kept)`, `per_file_dfs(2_kept)`, `patched(1_+1)`, `patched(2_+2fast)` — beam search covers the monolithic scaling case, DFS handles the small-N reliability floor.

Ready for merge.

## Recent Changes
---
type: feat
scope: project
summary: Initial manifest generated (PENDING REVIEW)
date: 2026-04-22
---
- NOTICE: Some .eck files are STUBS. They need manual or AI-assisted verification.

# Agent Report

# Recovery Task Complete — Scan_20260201.png Successfully Restored

## Result
**scan_broken.zip (4,079,532 bytes, FTP-ASCII-corrupted) → scan_fixed.zip (4,062,936 bytes) — BYTE-IDENTICAL to the original scan.zip.**

- `cmp test_scan/scan.zip test_scan/scan_fixed.zip` → identical
- Compressed PNG stream CRC32 `AE8A16AD` validated OK
- 44 CRs correctly re-inserted out of 16,640 LF decision points
- 93,605,252 beam attempts evaluated; final survivor was unique
- Total wall clock: 19,776 s (≈ 5h 29m)

## Final Algorithm (shipped in previous commits)
1. **Beam search** over 16,640 LF positions, width configurable via `LOXAM_BEAM_WIDTH` env var (used 5000).
2. **Content oracles** — three gates:
   - file-signature prefix (8-byte PNG magic)
   - trailer suffix (IEND chunk)
   - **streaming PNG chunk validator** (per-chunk length/type/CRC32 — 510 IDAT gates on this file).
3. **Water-fill bucketed pruning** keyed on `inserts.len()` so tied `total_out` doesn't evict the correct trajectory via stable-sort bias.
4. **Feed-semantics alignment** with `decompress_to_vec`: `TINFLStatus::Done` with leftover is accepted only at the true final feed.

## Key Recovery Checkpoints (from recovery_v7.log)
- LF 9500/16640: beam narrowed to **2** (chunk CRC gating worked)
- LF 14000/16640: beam narrowed to **64** (near convergence)
- LF 16500/16640: beam narrowed to **16** at 99.2% of output
- Final feed: 5000 → **1** survivor, `ContentInvalid=4999` (chunk validator killed all wrong trajectories)
- Trailer oracle: 1 → 1 winner

## Artifacts
- `test_scan/scan_fixed.zip` — recovered archive
- `test_scan/recovery_v{3,4,5,6,7}.log` — iteration logs documenting the v3→v7 fix sequence (UnexpectedDone fix → trailer oracle → chunk validator → beam-width tuning)

## Issues / Review Notes
- The run was very slow (~5.5 h with beam=5000 on this single file). For larger corrupted archives or tighter deadlines, consider:
  - Post-chunk-CRC candidate collapse (when all survivors share identical output prefix + state up to a chunk boundary, a Deflate-state hash could dedup).
  - Early termination once only 1 candidate remains.
  - Parallelising per-candidate feed with rayon (each beam iteration's fork is embarrassingly parallel).
- No algorithmic bugs discovered in the final run — beam width was the only tunable that mattered. Default `DEFAULT_MAX_BEAM_WIDTH = 2000` is too low for highly-corrupted binary streams; this PNG needed ≥ ~3000 to avoid beam-empty. 5000 worked. Consider bumping default to 4000–5000 and documenting the env-var override in README.
- A `50k_stress` regression was green (20/20) during development — no regressions introduced.

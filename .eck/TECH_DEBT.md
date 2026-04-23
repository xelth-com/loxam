# Loxam Tech Debt

## Resolved
- **`flate2` did not expose cloneable partial-decode state** — replaced with `miniz_oxide`'s `DecompressorOxide`
  (`Clone`-able since 0.8), enabling the Milestone 4 beam search to fork decompressor + CRC state at each CRLF.
- **Naive O(N^3) combinatorial search** — removed `try_brute_force` and `fix_section`'s keep_three loop.
  Replaced with Deflate-aware DFS backtracking that prunes at the first invalid Huffman block.
- **O(N*M) `build_candidate` with linear search** — replaced with `CrlfLookup` index array for O(N) builds.
- **Full-file Vec cloning on every attempt** — eliminated via in-place mutation and slice-based streaming.

## Active
- **No automated test suite** — only CLI `stress` command for verification. Need `#[test]` unit tests.
- **Single-threaded recovery** — per-file sections are independent and could be parallelized with rayon.
- **Beam search pruning heuristic is probabilistic** — ranking by `total_out DESC` can evict the correct candidate
  on small streams before miniz's lenient Huffman decoder flags it. DFS fallback masks this for now; a better
  signal (e.g. distance-code validity, deflate block boundary detection) would let us drop the fallback.

## Future
- ZIP64 support (files >4GB, >65535 entries)
- Streaming I/O (mmap or buffered reader) instead of loading entire file into memory
- Configurable DFS depth limit and timeout for adversarial inputs

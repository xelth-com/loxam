# Loxam Tech Debt

## Resolved
- **Naive O(N^3) combinatorial search** — removed `try_brute_force` and `fix_section`'s keep_three loop.
  Replaced with Deflate-aware DFS backtracking that prunes at the first invalid Huffman block.
- **O(N*M) `build_candidate` with linear search** — replaced with `CrlfLookup` index array for O(N) builds.
- **Full-file Vec cloning on every attempt** — eliminated via in-place mutation and slice-based streaming.

## Active
- **No automated test suite** — only CLI `stress` command for verification. Need `#[test]` unit tests.
- **Single-threaded recovery** — per-file sections are independent and could be parallelized with rayon.
- **flate2 does not expose partial-decode state** — DFS backtracking re-feeds from last checkpoint rather than
  saving/restoring internal decompressor state. A custom inflate wrapper would eliminate redundant work.

## Future
- ZIP64 support (files >4GB, >65535 entries)
- Streaming I/O (mmap or buffered reader) instead of loading entire file into memory
- Configurable DFS depth limit and timeout for adversarial inputs

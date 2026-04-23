# Loxam Roadmap


## Milestone 4: Stateful Beam Search for Monolithic Archives (DONE)
- Replaced `flate2` with `miniz_oxide` to expose cloneable `DecompressorOxide` + `crc32fast::Hasher` state
- Each `BeamCandidate` holds a clone of decompressor state, rolling 32KB LZ77 window, streaming CRC32 hasher, and its CRLF keep/strip history
- At each LF position the beam forks (strip CR vs keep CR); inter-LF bytes are fed streaming; candidates that hit `TINFLStatus::Failed` are pruned immediately
- Beam width capped at 2000, ranked by `total_out DESC` then `inserts ASC`, to survive the combinatorial explosion on archives with a single monolithic compressed payload
- DFS `keep_one` / `keep_two` kept as safety net for small/medium sections where beam's probabilistic pruning can drop the correct path
## Milestone 1: Core Recovery (DONE)
- ZIP creation, corruption (`\n` → `\r\n`), and basic recovery via "remove all CR" strategy
- Per-file CRC32 validation with keep_one/keep_two/keep_three patching
- CrlfLookup optimization for O(N) candidate building
- Stress-tested: 20/20 on 50KB files

## Milestone 2: Deflate-Aware DFS Backtracking (DONE)
- Replace naive O(N^3) combinatorial search with streaming Deflate backtracking
- Feed corrupted stream chunk-by-chunk into DeflateDecoder; backtrack at first invalid Huffman block
- Memory-mapped I/O for 380MB+ files (avoid full-file clone)
- Target: recover 380MB ZIP with ~6000 natural CRLFs in <60s
- Header-First Stabilization: memchr-based PK\x03\x04 scan anchors deflate replay, preventing cascade failures

## Milestone 3: Production Hardening (DONE)
- CLI progress bar for large file recovery (indicatif)
- Handle ZIP64 extended format (0x0001 extra field, u64 sizes)
- Parallel per-file recovery via rayon
- Zero-copy I/O via memmap2
- Memory-optimized: removed O(N) CrlfLookup and O(N) offset_map, replaced with O(1)-amortized merge scan and O(log M) prefix-sum mapping
- Stress-tested: 100/100 at 500B, 19/20 at 50KB

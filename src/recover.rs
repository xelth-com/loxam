use crate::zip_util;
use anyhow::{bail, Result};
use memchr::memmem;
use std::io::Read;
use std::time::Instant;

/// Result of a successful ZIP recovery attempt.
pub struct RecoveryResult {
    pub data: Vec<u8>,
    pub strategy: String,
    pub attempts: u64,
}

/// Entry point: recover a ZIP archive corrupted by `\n` → `\r\n` text-mode transfer.
///
/// Strategy cascade:
/// 1. **Remove-all** — strip every `\r` preceding `\n` (correct for ~99.6% of CRLFs).
/// 2. **Header-First Stabilization** — `PK\x03\x04` signatures survive corruption
///    intact (no 0x0A/0x0D bytes). For each signature, brute-force the small number
///    of CRLFs falling in the 16-byte critical field region (offsets 14-29) and
///    accept only combinations that place the next ZIP signature exactly where the
///    reconstructed `compressed_size` says it should land. This fixes header fields
///    whose numeric values legitimately contain 0x0D 0x0A, which a blanket strip
///    would otherwise destroy.
/// 3. **Per-file DFS** — for files whose CRC32 still mismatches, use a Deflate-aware
///    search that tries inserting `\r` at each `\n` position and validates via full
///    decompression + CRC32. For 2+ insertions, uses decompressed-byte scoring to
///    identify promising candidates before falling back to exhaustive pair search.
/// 4. **Patching** — if the global candidate is still invalid, add back individual
///    `\r` bytes until the full archive validates.
pub fn recover(data: &[u8]) -> Result<RecoveryResult> {
    let crlf_positions = zip_util::find_crlf_positions(data);

    if crlf_positions.is_empty() {
        if validate_as_zip(data) {
            return Ok(RecoveryResult {
                data: data.to_vec(),
                strategy: "no_corruption".to_string(),
                attempts: 0,
            });
        }
        bail!("No CRLF sequences found and data is not a valid ZIP");
    }

    eprintln!(
        "Found {} CRLF positions in {} bytes",
        crlf_positions.len(),
        data.len()
    );

    let t = Instant::now();
    let lookup = CrlfLookup::new(&crlf_positions, data.len());

    if let Some(result) = try_remove_all(data, &crlf_positions, &lookup) {
        eprintln!("  Done in {:.2}s", t.elapsed().as_secs_f64());
        return Ok(result);
    }

    eprintln!(
        "'Remove all' not valid ({:.2}s), running header stabilization...",
        t.elapsed().as_secs_f64()
    );

    let header_kept = stabilize_headers(data, &crlf_positions, &lookup);

    if let Some(result) = try_per_file_fix(data, &crlf_positions, &lookup, &header_kept) {
        eprintln!("  Done in {:.2}s", t.elapsed().as_secs_f64());
        return Ok(result);
    }

    bail!(
        "Recovery failed. {} CRLF positions, tried all strategies.",
        crlf_positions.len()
    )
}

fn validate_as_zip(data: &[u8]) -> bool {
    if !zip_util::is_valid_zip_signature(data) {
        return false;
    }
    match zip_util::parse_and_validate(data) {
        Ok(parsed) => parsed.entries.iter().all(|e| {
            e.crc32_actual.map_or(false, |actual| actual == e.crc32_expected)
        }) && !parsed.entries.is_empty(),
        Err(_) => false,
    }
}

struct CrlfLookup {
    index_at: Vec<Option<usize>>,
}

impl CrlfLookup {
    fn new(crlf_positions: &[usize], data_len: usize) -> Self {
        let mut index_at = vec![None::<usize>; data_len];
        for (idx, &pos) in crlf_positions.iter().enumerate() {
            index_at[pos] = Some(idx);
        }
        CrlfLookup { index_at }
    }
}

fn build_candidate_fast(data: &[u8], lookup: &CrlfLookup, remove_mask: &[bool]) -> Vec<u8> {
    let remove_count = remove_mask.iter().filter(|&&r| r).count();
    let mut result = Vec::with_capacity(data.len() - remove_count);
    for i in 0..data.len() {
        if let Some(idx) = lookup.index_at[i] {
            if remove_mask[idx] {
                continue;
            }
        }
        result.push(data[i]);
    }
    result
}

/// Map candidate-index → corrupted-index for a given remove-mask.
///
/// For each corrupted byte, include it in the map unless it's the CR of a CRLF
/// whose mask entry is `true` (i.e., the CR is being stripped). The old all-CR-stripped
/// behavior is recovered by passing a mask that's all-true.
fn build_offset_map(corrupted: &[u8], crlf_positions: &[usize], mask: &[bool]) -> Vec<usize> {
    let mut map = Vec::with_capacity(corrupted.len());
    let mut ci = 0;
    for i in 0..corrupted.len() {
        if ci < crlf_positions.len() && crlf_positions[ci] == i {
            let removed = mask[ci];
            ci += 1;
            if removed {
                continue;
            }
        }
        map.push(i);
    }
    map
}

fn try_decompress_check(data: &[u8], expected_crc: u32) -> bool {
    let mut decoder = flate2::read::DeflateDecoder::new(data);
    let mut result = Vec::new();
    if decoder.read_to_end(&mut result).is_err() {
        return false;
    }
    zip_util::crc32(&result) == expected_crc
}

/// Count how many bytes the Deflate decoder produces before encountering an error.
/// Used exclusively for ranking candidates when exhaustive search is too expensive.
fn try_decompress_count(data: &[u8]) -> usize {
    let mut decoder = flate2::read::DeflateDecoder::new(data);
    let mut buf = [0u8; 8192];
    let mut total = 0usize;
    loop {
        match decoder.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(_) => break,
        }
    }
    total
}

fn build_with_insertions(base: &[u8], insertions: &[usize]) -> Vec<u8> {
    let mut result = Vec::with_capacity(base.len() + insertions.len());
    let mut ins_idx = 0;
    for (i, &byte) in base.iter().enumerate() {
        while ins_idx < insertions.len() && insertions[ins_idx] == i {
            result.push(0x0D);
            ins_idx += 1;
        }
        result.push(byte);
    }
    result
}

fn try_remove_all(data: &[u8], crlf_positions: &[usize], lookup: &CrlfLookup) -> Option<RecoveryResult> {
    eprintln!("Strategy: remove all CR before LF");
    let remove_all: Vec<bool> = crlf_positions.iter().map(|_| true).collect();
    let candidate = build_candidate_fast(data, lookup, &remove_all);

    eprintln!(
        "  Candidate: {} bytes (corrupted: {}, removed {})",
        candidate.len(), data.len(), crlf_positions.len()
    );

    if validate_as_zip(&candidate) {
        return Some(RecoveryResult {
            data: candidate,
            strategy: "remove_all".to_string(),
            attempts: 1,
        });
    }
    None
}

/// Header-First Stabilization.
///
/// Why: FTP text-mode corruption (`\n` → `\r\n`) cannot create or split a
/// `PK\x03\x04` Local File Header signature — those 4 bytes contain neither 0x0A
/// nor 0x0D, so their exact positions in the corrupted byte stream are preserved.
/// However, the 16-byte region at header offsets 14-29 (crc32, compressed_size,
/// uncompressed_size, name_len, extra_len) holds arbitrary numeric data that may
/// legitimately contain 0x0A bytes. After corruption, each of those original 0x0A
/// bytes becomes `0x0D 0x0A` — indistinguishable from an inserted CR. A blanket
/// "strip every CR before LF" pass then mangles those fields and desynchronizes
/// every downstream offset (name_len, extra_len, compressed_size), making any
/// per-file payload repair impossible.
///
/// What: For each `PK\x03\x04` signature found in the raw corrupted data (via
/// `memchr::memmem`), this function:
///   1. Locates CRLF pairs in the forward window covering the 16 critical field
///      bytes (up to 32 corrupted bytes in the worst case where every original
///      field byte was 0x0A).
///   2. Enumerates all $2^N$ keep/strip combinations for those CRLFs, iterating
///      in order of ascending Hamming weight (fewest-kept first).
///   3. For each combination, reconstructs the 16 original field bytes and
///      computes `header_end + compressed_size` in corrupted coordinates.
///   4. Accepts the combination as valid only when some later PK signature
///      (`PK\x03\x04`, `PK\x01\x02`, or `PK\x07\x08`) lies at an offset that's
///      reachable given the CRLFs in the payload region — i.e., the "extra"
///      corrupted bytes between the predicted end and the next signature equal
///      the number of natural CRLFs in the payload, which must be at most the
///      total CRLFs observed in that range.
///
/// Returns: the set of CRLF indices (into `crlf_positions`) whose CR must be
/// preserved when building the base candidate for downstream payload DFS.
/// An empty result means every header worked under the blanket "remove all"
/// assumption; the caller proceeds as before.
fn stabilize_headers(
    data: &[u8],
    crlf_positions: &[usize],
    lookup: &CrlfLookup,
) -> Vec<usize> {
    let lfh_sigs: Vec<usize> = memmem::find_iter(data, b"PK\x03\x04").collect();
    let mut all_sigs: Vec<usize> = lfh_sigs.clone();
    all_sigs.extend(memmem::find_iter(data, b"PK\x01\x02"));
    all_sigs.extend(memmem::find_iter(data, b"PK\x07\x08"));
    all_sigs.sort();
    all_sigs.dedup();

    eprintln!(
        "Header stabilization: {} PK\\x03\\x04, {} total signatures",
        lfh_sigs.len(),
        all_sigs.len()
    );

    let mut kept_indices: Vec<usize> = Vec::new();

    for &sig_off in &lfh_sigs {
        let region_start = sig_off + 14;
        // Worst case: all 16 original field bytes were 0x0A, each now preceded by CR.
        let region_end_max = (sig_off + 14 + 32).min(data.len());
        if region_start >= data.len() {
            continue;
        }

        let lo = crlf_positions.partition_point(|&p| p < region_start);
        let hi = crlf_positions.partition_point(|&p| p < region_end_max);
        let region_crlf_indices: Vec<usize> = (lo..hi).collect();

        let n = region_crlf_indices.len();
        if n > 12 {
            eprintln!(
                "  sig@0x{:X}: too many CRLFs in critical region ({}), skipping",
                sig_off, n
            );
            continue;
        }

        // Enumerate masks in order of increasing Hamming weight: prefer fewer kept
        // CRLFs, so "remove-all" is tried first and we only start preserving CRs
        // when the strict oracle demands it.
        let mut masks: Vec<u32> = (0..(1u32 << n)).collect();
        masks.sort_by_key(|&m| m.count_ones());

        let mut found_for_sig: Option<Vec<usize>> = None;

        'mask_loop: for mask in masks {
            let kept_here: Vec<usize> = (0..n)
                .filter(|&b| (mask >> b) & 1 == 1)
                .map(|b| region_crlf_indices[b])
                .collect();

            let mut fields = [0u8; 16];
            let mut count = 0usize;
            let mut p = region_start;
            while count < 16 && p < data.len() {
                if let Some(idx) = lookup.index_at[p] {
                    if !kept_here.contains(&idx) {
                        p += 1;
                        continue;
                    }
                }
                fields[count] = data[p];
                count += 1;
                p += 1;
            }
            if count < 16 {
                continue;
            }

            // Any CRLF flagged "keep" that's past the reconstruction cursor is
            // inconsistent with this mask — it would have to live in filename/
            // extra territory, not the critical region.
            for &idx in &kept_here {
                if crlf_positions[idx] >= p {
                    continue 'mask_loop;
                }
            }

            let comp_size = u32::from_le_bytes(fields[4..8].try_into().unwrap()) as usize;
            let name_len = u16::from_le_bytes(fields[12..14].try_into().unwrap()) as usize;
            let extra_len = u16::from_le_bytes(fields[14..16].try_into().unwrap()) as usize;

            if comp_size > data.len() || name_len == 0 || name_len > 512 || extra_len > 512 {
                continue;
            }

            let name_extra_end = p + name_len + extra_len;
            if name_extra_end > data.len() {
                continue;
            }

            // Simplifying assumption: filenames in this codebase and typical ZIPs
            // do not contain 0x0A. If any CRLF lives in the filename/extra window,
            // this mask's offsets are ambiguous — skip rather than guess.
            let name_lo = crlf_positions.partition_point(|&cp| cp < p);
            let name_hi = crlf_positions.partition_point(|&cp| cp < name_extra_end);
            if name_lo != name_hi {
                continue;
            }

            let lb = name_extra_end + comp_size;
            if lb > data.len() {
                continue;
            }

            // Oracle: does some later PK signature lie at an offset reachable
            // given the CRLFs in the payload? Each "extra" corrupted byte beyond
            // `lb` must correspond to a natural CRLF, and natural CRLFs cannot
            // exceed the total CRLFs counted in the payload window.
            let payload_crlf_lo = crlf_positions.partition_point(|&cp| cp < name_extra_end);
            let sig_lo = all_sigs.partition_point(|&s| s < lb);
            let mut valid = false;
            for &next_sig in &all_sigs[sig_lo..] {
                let extra_bytes = next_sig - lb;
                let payload_crlf_hi = crlf_positions.partition_point(|&cp| cp < next_sig);
                let total_crlfs_in_payload = payload_crlf_hi - payload_crlf_lo;
                if extra_bytes <= total_crlfs_in_payload {
                    valid = true;
                    break;
                }
                // next_sig is too far; a further one can only be even further,
                // but it might still fit if the payload has many CRLFs. Continue
                // scanning rather than breaking — loop stops when we exhaust
                // sigs or find a match.
            }

            if valid {
                found_for_sig = Some(kept_here);
                break;
            }
        }

        if let Some(kept) = found_for_sig {
            if !kept.is_empty() {
                eprintln!(
                    "  sig@0x{:X}: kept {} CRLF(s) in critical region",
                    sig_off,
                    kept.len()
                );
            }
            kept_indices.extend(kept);
        }
    }

    kept_indices.sort();
    kept_indices.dedup();
    eprintln!(
        "  stabilization decided to keep {} CRLF(s) across all headers",
        kept_indices.len()
    );
    kept_indices
}

fn try_per_file_fix(
    data: &[u8],
    crlf_positions: &[usize],
    lookup: &CrlfLookup,
    initial_kept: &[usize],
) -> Option<RecoveryResult> {
    let mut base_mask: Vec<bool> = crlf_positions.iter().map(|_| true).collect();
    for &idx in initial_kept {
        if idx < base_mask.len() {
            base_mask[idx] = false;
        }
    }
    let candidate = build_candidate_fast(data, lookup, &base_mask);

    let parsed = match zip_util::parse_and_validate(&candidate) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("  Cannot parse stabilized candidate: {}", e);
            return try_fix_global(data, crlf_positions, lookup);
        }
    };

    let bad_indices: Vec<usize> = parsed
        .entries
        .iter()
        .enumerate()
        .filter(|(_, e)| e.crc32_actual.map_or(true, |a| a != e.crc32_expected))
        .map(|(i, _)| i)
        .collect();

    if bad_indices.is_empty() {
        let strategy = if initial_kept.is_empty() {
            "remove_all".to_string()
        } else {
            format!("header_stabilized({}_kept)", initial_kept.len())
        };
        return Some(RecoveryResult {
            data: candidate,
            strategy,
            attempts: 1,
        });
    }

    eprintln!(
        "  {} file(s) need fix: {:?}",
        bad_indices.len(),
        bad_indices.iter().map(|&i| parsed.entries[i].name.as_str()).collect::<Vec<_>>()
    );

    let offset_map = build_offset_map(data, crlf_positions, &base_mask);

    let mut file_comp_bounds: Vec<(usize, usize)> = Vec::new();
    let mut pos = 0;
    for entry in &parsed.entries {
        let header_size = 30 + entry.name.len();
        let comp_start = pos + header_size;
        let comp_end = comp_start + entry.compressed_size as usize;
        file_comp_bounds.push((comp_start, comp_end));
        pos = comp_end;
    }

    let mut kept_crlf_indices: Vec<usize> = initial_kept.to_vec();
    let mut total_attempts = 0u64;

    for &file_idx in &bad_indices {
        let entry = &parsed.entries[file_idx];
        let (comp_start, comp_end) = file_comp_bounds[file_idx];
        let compressed_data = &candidate[comp_start..comp_end];

        eprintln!(
            "  '{}': {} bytes compressed, CRC {:08X}",
            entry.name, compressed_data.len(), entry.crc32_expected
        );

        let lf_positions: Vec<usize> = compressed_data
            .iter()
            .enumerate()
            .filter(|&(_, &b)| b == 0x0A)
            .map(|(i, _)| i)
            .collect();

        eprintln!("    {} LF positions in compressed data", lf_positions.len());

        let found = dfs_fix_section(
            compressed_data,
            entry.crc32_expected,
            &lf_positions,
            &mut total_attempts,
        );

        match found {
            None => {
                eprintln!("    Could not fix '{}'", entry.name);
                return None;
            }
            Some(insert_positions) => {
                for rel_pos in &insert_positions {
                    let cand_pos = comp_start + rel_pos;
                    if cand_pos >= offset_map.len() {
                        eprintln!("    BUG: cand_pos {} >= offset_map.len() {}", cand_pos, offset_map.len());
                        return None;
                    }
                    let corr_pos = offset_map[cand_pos];
                    if corr_pos == 0 {
                        eprintln!("    BUG: corr_pos is 0 for cand_pos {}", cand_pos);
                        return None;
                    }
                    let crlf_corr_pos = corr_pos - 1;
                    if let Some(crlf_idx) = crlf_positions.iter().position(|&p| p == crlf_corr_pos) {
                        kept_crlf_indices.push(crlf_idx);
                    } else {
                        eprintln!(
                            "    WARNING: no CRLF at corrupted pos 0x{:X} (near candidate 0x{:X})",
                            crlf_corr_pos, cand_pos
                        );
                    }
                }
                eprintln!(
                    "    Fixed '{}': {} CRs restored",
                    entry.name, insert_positions.len()
                );
            }
        }
    }

    let mut final_mask: Vec<bool> = crlf_positions.iter().map(|_| true).collect();
    for &idx in &kept_crlf_indices {
        final_mask[idx] = false;
    }
    let candidate = build_candidate_fast(data, lookup, &final_mask);

    if !validate_as_zip(&candidate) {
        eprintln!("  Global candidate invalid, trying to patch remaining issues...");
        return try_patch_remaining(data, crlf_positions, lookup, &kept_crlf_indices);
    }

    Some(RecoveryResult {
        data: candidate,
        strategy: format!("per_file_dfs({}_kept)", kept_crlf_indices.len()),
        attempts: total_attempts,
    })
}

/// Deflate-aware section fix using exhaustive CRC validation for low orders and
/// decompressed-byte scoring to guide the search for higher orders.
///
/// Strategy:
/// - **keep_one**: try each LF position with full decompression + CRC32 check. O(N).
/// - **keep_two**: try all pairs with full check. O(N²). Feasible up to ~500 positions.
/// - **keep_three+**: score each position by decompressed byte count, pick the best,
///   then re-run from keep_one with that insertion locked in. This is a greedy DFS
///   that uses the Deflate decoder's sensitivity to wrong bytes to rank candidates
///   — but validates every result with a full CRC check before accepting.
fn dfs_fix_section(
    base: &[u8],
    expected_crc: u32,
    lf_positions: &[usize],
    total_attempts: &mut u64,
) -> Option<Vec<usize>> {
    if try_decompress_check(base, expected_crc) {
        return Some(vec![]);
    }

    let t = Instant::now();
    let n = lf_positions.len();

    // keep_one: full CRC validation
    for (k, &pos) in lf_positions.iter().enumerate() {
        *total_attempts += 1;
        let mut test = base.to_vec();
        test.insert(pos, 0x0D);
        if try_decompress_check(&test, expected_crc) {
            eprintln!(
                "    keep_one at LF#{} (pos {}), {:.2}s",
                k, pos, t.elapsed().as_secs_f64()
            );
            return Some(vec![pos]);
        }
    }
    eprintln!("    keep_one failed ({}, {:.2}s)", n, t.elapsed().as_secs_f64());

    // keep_two: full CRC validation for all pairs
    let t2 = Instant::now();
    for i in 0..n {
        for j in (i + 1)..n {
            *total_attempts += 1;
            let mut test = base.to_vec();
            test.insert(lf_positions[j], 0x0D);
            test.insert(lf_positions[i], 0x0D);
            if try_decompress_check(&test, expected_crc) {
                eprintln!(
                    "    keep_two at LF#{} + #{}, {:.2}s",
                    i, j, t2.elapsed().as_secs_f64()
                );
                return Some(vec![lf_positions[i], lf_positions[j]]);
            }
        }
    }
    eprintln!("    keep_two failed ({}, {:.2}s)", n * (n - 1) / 2, t2.elapsed().as_secs_f64());

    // keep_three+: score each candidate by how much decompressed output it yields,
    // then try the top-K candidates with full validation in deeper combinations.
    let t3 = Instant::now();
    let mut scored: Vec<(usize, usize)> = Vec::with_capacity(n);
    let base_score = try_decompress_count(base);

    for (i, &pos) in lf_positions.iter().enumerate() {
        *total_attempts += 1;
        let mut test = base.to_vec();
        test.insert(pos, 0x0D);
        let score = try_decompress_count(&test);
        if score > base_score {
            scored.push((i, score));
        }
    }
    scored.sort_by(|a, b| b.1.cmp(&a.1));

    let top_k = 30.min(scored.len());
    eprintln!(
        "    Scoring: {}/{} candidates improved output, trying top-{} for keep_three ({:.2}s)",
        scored.len(), n, top_k, t3.elapsed().as_secs_f64()
    );

    for a in 0..top_k {
        for b in (a + 1)..top_k {
            for c in (b + 1)..top_k {
                *total_attempts += 1;
                let mut test = base.to_vec();
                let mut poses = vec![lf_positions[scored[a].0], lf_positions[scored[b].0], lf_positions[scored[c].0]];
                poses.sort();
                for (idx, &p) in poses.iter().enumerate() {
                    test.insert(p + idx, 0x0D);
                }
                if try_decompress_check(&test, expected_crc) {
                    eprintln!(
                        "    keep_three (scored top-{}/{}/{}) {:.2}s",
                        a, b, c, t3.elapsed().as_secs_f64()
                    );
                    return Some(poses);
                }
            }
        }
    }

    eprintln!("    keep_three failed ({:.2}s)", t3.elapsed().as_secs_f64());
    None
}

fn try_patch_remaining(
    data: &[u8],
    crlf_positions: &[usize],
    lookup: &CrlfLookup,
    already_kept: &[usize],
) -> Option<RecoveryResult> {
    let kept_set: std::collections::HashSet<usize> = already_kept.iter().copied().collect();
    let remaining: Vec<usize> = (0..crlf_positions.len())
        .filter(|i| !kept_set.contains(i))
        .collect();

    eprintln!("  {} CRLFs not yet decided, trying to add more...", remaining.len());

    let mut mask: Vec<bool> = crlf_positions.iter().map(|_| true).collect();
    for &idx in already_kept {
        mask[idx] = false;
    }

    for &ri in &remaining {
        mask[ri] = false;
        let candidate = build_candidate_fast(data, lookup, &mask);
        if validate_as_zip(&candidate) {
            eprintln!("  Fixed by additionally keeping CR at index {}", ri);
            return Some(RecoveryResult {
                data: candidate,
                strategy: format!("patched({}_+1)", already_kept.len()),
                attempts: 1,
            });
        }
        mask[ri] = true;
    }

    eprintln!(
        "  +1 failed, trying fast pair search on {} remaining...",
        remaining.len()
    );

    let base_candidate = build_candidate_fast(data, lookup, &mask);
    let corr_to_cand = build_corr_to_cand_map(data, crlf_positions, already_kept);

    let mut cand_insert_positions: Vec<(usize, usize)> = remaining
        .iter()
        .enumerate()
        .filter_map(|(i, &ri)| {
            let crlf_pos = crlf_positions[ri];
            let lf_pos = crlf_pos + 1;
            if lf_pos < corr_to_cand.len() && corr_to_cand[lf_pos] > 0 {
                Some((corr_to_cand[lf_pos], i))
            } else {
                None
            }
        })
        .collect();
    cand_insert_positions.sort_by_key(|&(pos, _)| pos);

    eprintln!("  {} valid insert positions", cand_insert_positions.len());

    let total_pairs = cand_insert_positions.len() * (cand_insert_positions.len() - 1) / 2;
    eprintln!("  {} pairs to try", total_pairs);

    let mut tested = 0u64;
    let report_interval = 10000u64;

    let mut test = base_candidate.clone();

    for a in 0..cand_insert_positions.len() {
        let (p1, idx_a) = cand_insert_positions[a];
        test.insert(p1, 0x0D);
        for b in (a + 1)..cand_insert_positions.len() {
            let (p2, idx_b) = cand_insert_positions[b];
            let adjusted_p2 = if p2 > p1 { p2 + 1 } else { p2 };
            test.insert(adjusted_p2, 0x0D);
            if validate_as_zip(&test) {
                eprintln!(
                    "  Fixed by keeping 2 CRs (remaining #{}, #{}, tested {})",
                    idx_a, idx_b, tested
                );
                let mut final_mask = mask.clone();
                final_mask[remaining[idx_a]] = false;
                final_mask[remaining[idx_b]] = false;
                let candidate = build_candidate_fast(data, lookup, &final_mask);
                return Some(RecoveryResult {
                    data: candidate,
                    strategy: format!("patched({}_+2fast)", already_kept.len()),
                    attempts: tested,
                });
            }
            test.remove(adjusted_p2);
            tested += 1;
            if tested % report_interval == 0 {
                eprintln!("    ...tested {} / {} pairs", tested, total_pairs);
            }
        }
        test.remove(p1);
    }

    eprintln!("  Patching failed (tested {} pairs)", tested);
    None
}

fn build_corr_to_cand_map(data: &[u8], crlf_positions: &[usize], already_kept: &[usize]) -> Vec<usize> {
    let crlf_set: std::collections::HashSet<usize> = crlf_positions.iter().copied().collect();
    let kept_crlf_set: std::collections::HashSet<usize> =
        already_kept.iter().map(|&i| crlf_positions[i]).collect();
    let mut map = vec![0usize; data.len()];
    let mut cand_pos = 0;
    for corr_pos in 0..data.len() {
        if crlf_set.contains(&corr_pos) && !kept_crlf_set.contains(&corr_pos) {
            // This CR was removed, skip it
        } else {
            map[corr_pos] = cand_pos;
            cand_pos += 1;
        }
    }
    map
}

fn try_fix_global(data: &[u8], crlf_positions: &[usize], lookup: &CrlfLookup) -> Option<RecoveryResult> {
    eprintln!("  Trying global keep-one...");
    let mut mask: Vec<bool> = crlf_positions.iter().map(|_| true).collect();
    for i in 0..crlf_positions.len() {
        mask[i] = false;
        let candidate = build_candidate_fast(data, lookup, &mask);
        if validate_as_zip(&candidate) {
            return Some(RecoveryResult {
                data: candidate,
                strategy: format!("keep_one_global_{}", i),
                attempts: (i + 1) as u64,
            });
        }
        mask[i] = true;
    }
    None
}

fn try_global_keep_n(
    data: &[u8],
    crlf_positions: &[usize],
    lookup: &CrlfLookup,
    n: usize,
) -> Option<RecoveryResult> {
    let total = crlf_positions.len();
    if n == 0 || total < n {
        return None;
    }

    eprintln!("  Trying global keep-{} (remove all but {} of {})...", n, n, total);

    let remove_all_mask: Vec<bool> = crlf_positions.iter().map(|_| true).collect();
    let remove_all_candidate = build_candidate_fast(data, lookup, &remove_all_mask);

    let corr_to_cand = build_corr_to_cand_map(data, crlf_positions, &[]);
    let cand_positions: Vec<usize> = crlf_positions
        .iter()
        .filter_map(|&crlf_pos| {
            let lf_pos = crlf_pos + 1;
            if lf_pos < corr_to_cand.len() {
                Some(corr_to_cand[lf_pos])
            } else {
                None
            }
        })
        .collect();

    let mut combo = Vec::new();
    try_global_keep_n_recursive(
        data,
        lookup,
        &remove_all_candidate,
        &cand_positions,
        n,
        0,
        &mut combo,
    )
}

fn try_global_keep_n_recursive(
    data: &[u8],
    lookup: &CrlfLookup,
    base_candidate: &[u8],
    cand_positions: &[usize],
    depth: usize,
    start: usize,
    combo: &mut Vec<usize>,
) -> Option<RecoveryResult> {
    if combo.len() == depth {
        let mut test = base_candidate.to_vec();
        let mut positions: Vec<usize> = combo.iter().map(|&i| cand_positions[i]).collect();
        positions.sort();
        for (shift, &pos) in positions.iter().enumerate() {
            test.insert(pos + shift, 0x0D);
        }
        if validate_as_zip(&test) {
            let mut mask: Vec<bool> = vec![true; cand_positions.len()];
            for &i in combo.iter() {
                mask[i] = false;
            }
            let candidate = build_candidate_fast(data, lookup, &mask);
            eprintln!("    Global keep-{} found!", depth);
            return Some(RecoveryResult {
                data: candidate,
                strategy: format!("global_keep_{}", depth),
                attempts: 1,
            });
        }
        return None;
    }

    for i in start..cand_positions.len() {
        combo.push(i);
        let result = try_global_keep_n_recursive(
            data, lookup, base_candidate, cand_positions, depth, i + 1, combo,
        );
        if result.is_some() {
            return result;
        }
        combo.pop();
    }
    None
}

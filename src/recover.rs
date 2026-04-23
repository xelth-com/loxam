use crate::zip_util;
use anyhow::{bail, Result};
use indicatif::{ProgressBar, ProgressStyle};
use memchr::memmem;
use rayon::prelude::*;
use std::io::Read;
use std::time::Instant;

pub struct RecoveryResult {
    pub data: Vec<u8>,
    pub strategy: String,
    pub attempts: u64,
}

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

    if let Some(result) = try_remove_all(data, &crlf_positions) {
        eprintln!("  Done in {:.2}s", t.elapsed().as_secs_f64());
        return Ok(result);
    }

    eprintln!(
        "'Remove all' not valid ({:.2}s), running header stabilization...",
        t.elapsed().as_secs_f64()
    );

    let header_kept = stabilize_headers(data, &crlf_positions);

    if let Some(result) = try_per_file_fix(data, &crlf_positions, &header_kept) {
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
        Ok(parsed) => parsed
            .entries
            .iter()
            .all(|e| e.crc32_actual.map_or(false, |actual| actual == e.crc32_expected))
            && !parsed.entries.is_empty(),
        Err(_) => false,
    }
}

fn build_candidate(data: &[u8], crlf_positions: &[usize], remove_mask: &[bool]) -> Vec<u8> {
    let remove_count = remove_mask.iter().filter(|&&r| r).count();
    let mut result = Vec::with_capacity(data.len().saturating_sub(remove_count));
    let mut ci = 0;
    for (i, &byte) in data.iter().enumerate() {
        if ci < crlf_positions.len() && crlf_positions[ci] == i {
            if remove_mask[ci] {
                ci += 1;
                continue;
            }
            ci += 1;
        }
        result.push(byte);
    }
    result
}

fn build_removed_prefix(mask: &[bool]) -> Vec<usize> {
    let mut prefix = Vec::with_capacity(mask.len() + 1);
    prefix.push(0);
    let mut count = 0;
    for &removed in mask {
        if removed {
            count += 1;
        }
        prefix.push(count);
    }
    prefix
}

fn cand_to_corr(cand_pos: usize, crlf_positions: &[usize], removed_prefix: &[usize]) -> usize {
    let mut r = 0usize;
    for _ in 0..20 {
        let target = cand_pos + r;
        let idx = crlf_positions.partition_point(|&p| p < target);
        let new_r = removed_prefix[idx];
        if new_r == r {
            if idx < crlf_positions.len()
                && crlf_positions[idx] == target
                && removed_prefix.get(idx + 1).map_or(false, |&v| v > removed_prefix[idx])
            {
                r += 1;
                continue;
            }
            return target;
        }
        r = new_r;
    }
    cand_pos + r
}

fn corr_to_cand(corr_pos: usize, crlf_positions: &[usize], removed_prefix: &[usize]) -> usize {
    let idx = crlf_positions.partition_point(|&p| p < corr_pos);
    let removed_before = removed_prefix[idx];
    corr_pos - removed_before
}

fn try_decompress_check(data: &[u8], expected_crc: u32) -> bool {
    let mut decoder = flate2::read::DeflateDecoder::new(data);
    let mut result = Vec::new();
    if decoder.read_to_end(&mut result).is_err() {
        return false;
    }
    zip_util::crc32(&result) == expected_crc
}

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

fn try_remove_all(data: &[u8], crlf_positions: &[usize]) -> Option<RecoveryResult> {
    eprintln!("Strategy: remove all CR before LF");
    let remove_all: Vec<bool> = crlf_positions.iter().map(|_| true).collect();
    let candidate = build_candidate(data, crlf_positions, &remove_all);

    eprintln!(
        "  Candidate: {} bytes (corrupted: {}, removed {})",
        candidate.len(),
        data.len(),
        crlf_positions.len()
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

fn stabilize_headers(data: &[u8], crlf_positions: &[usize]) -> Vec<usize> {
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
            let mut crlf_cursor = lo;
            while count < 16 && p < data.len() {
                let is_crlf =
                    crlf_cursor < crlf_positions.len() && crlf_positions[crlf_cursor] == p;
                if is_crlf {
                    let idx = crlf_cursor;
                    crlf_cursor += 1;
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

            let name_lo = crlf_positions.partition_point(|&cp| cp < p);
            let name_hi = crlf_positions.partition_point(|&cp| cp < name_extra_end);
            if name_lo != name_hi {
                continue;
            }

            let lb = name_extra_end + comp_size;
            if lb > data.len() {
                continue;
            }

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
                    sig_off, kept.len()
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
    initial_kept: &[usize],
) -> Option<RecoveryResult> {
    let mut base_mask: Vec<bool> = crlf_positions.iter().map(|_| true).collect();
    for &idx in initial_kept {
        if idx < base_mask.len() {
            base_mask[idx] = false;
        }
    }
    let candidate = build_candidate(data, crlf_positions, &base_mask);

    let parsed = match zip_util::parse_and_validate(&candidate) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("  Cannot parse stabilized candidate: {}", e);
            return try_fix_global(data, crlf_positions);
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
        bad_indices
            .iter()
            .map(|&i| parsed.entries[i].name.as_str())
            .collect::<Vec<_>>()
    );

    let mut file_comp_bounds: Vec<(usize, usize)> = Vec::with_capacity(parsed.entries.len());
    let mut pos = 0;
    for entry in &parsed.entries {
        let header_size = 30 + entry.name.len();
        let comp_start = pos + header_size;
        let comp_end = comp_start + entry.compressed_size as usize;
        file_comp_bounds.push((comp_start, comp_end));
        pos = comp_end;
    }

    let pb = ProgressBar::new(bad_indices.len() as u64);
    pb.set_style(
        ProgressStyle::with_template(
            "{spinner:.green} [{bar:40.cyan/blue}] {pos}/{len} files ({per_sec}) {msg}",
        )
        .unwrap()
        .progress_chars("#>-"),
    );

    let results: Vec<(usize, Option<Vec<usize>>, u64)> = bad_indices
        .par_iter()
        .map(|&file_idx| {
            let entry = &parsed.entries[file_idx];
            let (comp_start, comp_end) = file_comp_bounds[file_idx];
            let compressed_data = &candidate[comp_start..comp_end];

            eprintln!(
                "  '{}': {} bytes compressed, CRC {:08X}",
                entry.name,
                compressed_data.len(),
                entry.crc32_expected
            );

            let lf_positions: Vec<usize> = compressed_data
                .iter()
                .enumerate()
                .filter(|&(_, &b)| b == 0x0A)
                .map(|(i, _)| i)
                .collect();

            eprintln!("    {} LF positions in compressed data", lf_positions.len());

            let mut attempts = 0u64;
            let result =
                dfs_fix_section(compressed_data, entry.crc32_expected, &lf_positions, &mut attempts);
            pb.inc(1);
            (file_idx, result, attempts)
        })
        .collect();

    pb.finish_with_message("done");

    let removed_prefix = build_removed_prefix(&base_mask);
    let mut kept_crlf_indices: Vec<usize> = initial_kept.to_vec();
    let mut total_attempts = 0u64;

    for (file_idx, result, attempts) in results {
        total_attempts += attempts;
        match result {
            None => {
                eprintln!("    Could not fix '{}'", parsed.entries[file_idx].name);
                return None;
            }
            Some(insert_positions) => {
                let (comp_start, _) = file_comp_bounds[file_idx];
                for rel_pos in &insert_positions {
                    let cand_pos = comp_start + rel_pos;
                    let corr_pos = cand_to_corr(cand_pos, crlf_positions, &removed_prefix);
                    if corr_pos == 0 {
                        eprintln!("    BUG: corr_pos is 0 for cand_pos {}", cand_pos);
                        return None;
                    }
                    let crlf_corr_pos = corr_pos - 1;
                    if let Ok(crlf_idx) = crlf_positions.binary_search(&crlf_corr_pos) {
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
                    parsed.entries[file_idx].name,
                    insert_positions.len()
                );
            }
        }
    }

    let mut final_mask: Vec<bool> = crlf_positions.iter().map(|_| true).collect();
    for &idx in &kept_crlf_indices {
        final_mask[idx] = false;
    }
    let candidate = build_candidate(data, crlf_positions, &final_mask);

    if !validate_as_zip(&candidate) {
        eprintln!("  Global candidate invalid, trying to patch remaining issues...");
        return try_patch_remaining(data, crlf_positions, &kept_crlf_indices);
    }

    Some(RecoveryResult {
        data: candidate,
        strategy: format!("per_file_dfs({}_kept)", kept_crlf_indices.len()),
        attempts: total_attempts,
    })
}

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

    let mut test = vec![0u8; base.len() + 1];
    for &pos in lf_positions {
        *total_attempts += 1;
        test[..pos].copy_from_slice(&base[..pos]);
        test[pos] = 0x0D;
        test[pos + 1..].copy_from_slice(&base[pos..]);
        if try_decompress_check(&test, expected_crc) {
            eprintln!("    keep_one at pos {}, {:.2}s", pos, t.elapsed().as_secs_f64());
            return Some(vec![pos]);
        }
    }
    eprintln!(
        "    keep_one failed ({}, {:.2}s)",
        n,
        t.elapsed().as_secs_f64()
    );

    if n <= 2000 {
        let t2 = Instant::now();
        let mut test = vec![0u8; base.len() + 2];
        for i in 0..n {
            for j in (i + 1)..n {
                *total_attempts += 1;
                let p1 = lf_positions[i];
                let p2 = lf_positions[j];
                test[..p1].copy_from_slice(&base[..p1]);
                test[p1] = 0x0D;
                test[p1 + 1..p2 + 1].copy_from_slice(&base[p1..p2]);
                test[p2 + 1] = 0x0D;
                test[p2 + 2..].copy_from_slice(&base[p2..]);
                if try_decompress_check(&test, expected_crc) {
                    eprintln!(
                        "    keep_two at {} + {}, {:.2}s",
                        p1, p2, t2.elapsed().as_secs_f64()
                    );
                    return Some(vec![p1, p2]);
                }
            }
        }
        eprintln!(
            "    keep_two failed ({}, {:.2}s)",
            n * (n - 1) / 2,
            t2.elapsed().as_secs_f64()
        );
    }

    let t3 = Instant::now();
    let base_score = try_decompress_count(base);
    let mut scored: Vec<(usize, usize)> = Vec::with_capacity(n);

    {
        let mut test = vec![0u8; base.len() + 1];
        for (i, &pos) in lf_positions.iter().enumerate() {
            *total_attempts += 1;
            test[..pos].copy_from_slice(&base[..pos]);
            test[pos] = 0x0D;
            test[pos + 1..].copy_from_slice(&base[pos..]);
            let score = try_decompress_count(&test);
            if score > base_score {
                scored.push((i, score));
            }
        }
    }
    scored.sort_by(|a, b| b.1.cmp(&a.1));

    let top_k = 30.min(scored.len());
    eprintln!(
        "    Scoring: {}/{} improved, top-{} for keep_three ({:.2}s)",
        scored.len(),
        n,
        top_k,
        t3.elapsed().as_secs_f64()
    );

    if top_k >= 3 {
        let mut test = vec![0u8; base.len() + 3];
        for a in 0..top_k {
            for b in (a + 1)..top_k {
                for c in (b + 1)..top_k {
                    *total_attempts += 1;
                    let mut poses = vec![
                        lf_positions[scored[a].0],
                        lf_positions[scored[b].0],
                        lf_positions[scored[c].0],
                    ];
                    poses.sort();
                    let p1 = poses[0];
                    let p2 = poses[1];
                    let p3 = poses[2];
                    test[..p1].copy_from_slice(&base[..p1]);
                    test[p1] = 0x0D;
                    test[p1 + 1..p2 + 1].copy_from_slice(&base[p1..p2]);
                    test[p2 + 1] = 0x0D;
                    test[p2 + 2..p3 + 2].copy_from_slice(&base[p2..p3]);
                    test[p3 + 2] = 0x0D;
                    test[p3 + 3..].copy_from_slice(&base[p3..]);
                    if try_decompress_check(&test, expected_crc) {
                        eprintln!(
                            "    keep_three ({}/{}/{}) {:.2}s",
                            a, b, c, t3.elapsed().as_secs_f64()
                        );
                        return Some(poses);
                    }
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
    already_kept: &[usize],
) -> Option<RecoveryResult> {
    if data.len() > 200_000_000 {
        eprintln!("  File too large for patching fallback, skipping");
        return None;
    }

    let kept_set: std::collections::HashSet<usize> = already_kept.iter().copied().collect();
    let remaining: Vec<usize> = (0..crlf_positions.len())
        .filter(|i| !kept_set.contains(i))
        .collect();

    eprintln!(
        "  {} CRLFs not yet decided, trying to add more...",
        remaining.len()
    );

    let mut mask: Vec<bool> = crlf_positions.iter().map(|_| true).collect();
    for &idx in already_kept {
        mask[idx] = false;
    }

    for &ri in &remaining {
        mask[ri] = false;
        let candidate = build_candidate(data, crlf_positions, &mask);
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

    let base_candidate = build_candidate(data, crlf_positions, &mask);
    let removed_prefix = build_removed_prefix(&mask);

    let mut cand_insert_positions: Vec<(usize, usize)> = remaining
        .iter()
        .enumerate()
        .filter_map(|(i, &ri)| {
            let crlf_pos = crlf_positions[ri];
            let lf_pos = crlf_pos + 1;
            let cand_lf = corr_to_cand(lf_pos, crlf_positions, &removed_prefix);
            if cand_lf > 0 {
                Some((cand_lf, i))
            } else {
                None
            }
        })
        .collect();
    cand_insert_positions.sort_by_key(|&(pos, _)| pos);

    eprintln!("  {} valid insert positions", cand_insert_positions.len());

    let total_pairs = cand_insert_positions.len() * cand_insert_positions.len().saturating_sub(1) / 2;
    eprintln!("  {} pairs to try", total_pairs);

    let mut tested = 0u64;
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
                let candidate = build_candidate(data, crlf_positions, &final_mask);
                return Some(RecoveryResult {
                    data: candidate,
                    strategy: format!("patched({}_+2fast)", already_kept.len()),
                    attempts: tested,
                });
            }
            test.remove(adjusted_p2);
            tested += 1;
            if tested % 10000 == 0 {
                eprintln!("    ...tested {} / {} pairs", tested, total_pairs);
            }
        }
        test.remove(p1);
    }

    eprintln!("  Patching failed (tested {} pairs)", tested);
    None
}

fn try_fix_global(data: &[u8], crlf_positions: &[usize]) -> Option<RecoveryResult> {
    eprintln!("  Trying global keep-one...");
    let mut mask: Vec<bool> = crlf_positions.iter().map(|_| true).collect();
    for i in 0..crlf_positions.len() {
        mask[i] = false;
        let candidate = build_candidate(data, crlf_positions, &mask);
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

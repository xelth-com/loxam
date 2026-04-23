use crate::zip_util;
use anyhow::{bail, Result};
use indicatif::{ProgressBar, ProgressStyle};
use memchr::memmem;
use miniz_oxide::inflate::core::{decompress, inflate_flags, DecompressorOxide};
use miniz_oxide::inflate::TINFLStatus;
use rayon::prelude::*;
use std::time::Instant;

const BEAM_OUT_BUF_SIZE: usize = 32 * 1024;
const MAX_BEAM_WIDTH: usize = 2000;

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
    match miniz_oxide::inflate::decompress_to_vec(data) {
        Ok(out) => zip_util::crc32(&out) == expected_crc,
        Err(_) => false,
    }
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
            let result = beam_search_fix_section(
                compressed_data,
                entry.crc32_expected,
                entry.uncompressed_size,
                &lf_positions,
                &mut attempts,
            )
            .or_else(|| {
                eprintln!("    beam failed, falling back to DFS");
                dfs_fix_section_fallback(
                    compressed_data,
                    entry.crc32_expected,
                    &lf_positions,
                    &mut attempts,
                )
            });
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

#[derive(Clone)]
struct BeamCandidate {
    state: Box<DecompressorOxide>,
    hasher: crc32fast::Hasher,
    out_buf: Vec<u8>,
    out_pos: usize,
    total_out: u64,
    inserts: Vec<usize>,
    done: bool,
}

impl BeamCandidate {
    fn new() -> Self {
        BeamCandidate {
            state: Box::new(DecompressorOxide::new()),
            hasher: crc32fast::Hasher::new(),
            out_buf: vec![0u8; BEAM_OUT_BUF_SIZE],
            out_pos: 0,
            total_out: 0,
            inserts: Vec::new(),
            done: false,
        }
    }

    fn feed(&mut self, chunk: &[u8], has_more_after: bool) -> Result<(), ()> {
        if self.done {
            return if chunk.is_empty() { Ok(()) } else { Err(()) };
        }

        let buf_len = self.out_buf.len();
        let mut in_pos = 0usize;

        loop {
            let more_input = has_more_after || in_pos < chunk.len();
            let flags = if more_input {
                inflate_flags::TINFL_FLAG_HAS_MORE_INPUT
            } else {
                0
            };

            let (status, in_consumed, out_consumed) = decompress(
                &mut *self.state,
                &chunk[in_pos..],
                &mut self.out_buf,
                self.out_pos,
                flags,
            );

            in_pos += in_consumed;

            if out_consumed > 0 {
                let start = self.out_pos;
                let end = start + out_consumed;
                if end <= buf_len {
                    self.hasher.update(&self.out_buf[start..end]);
                } else {
                    self.hasher.update(&self.out_buf[start..]);
                    self.hasher.update(&self.out_buf[..(end & (buf_len - 1))]);
                }
                self.out_pos = end & (buf_len - 1);
                self.total_out += out_consumed as u64;
            }

            match status {
                TINFLStatus::Done => {
                    self.done = true;
                    return if in_pos < chunk.len() { Err(()) } else { Ok(()) };
                }
                TINFLStatus::NeedsMoreInput => {
                    if in_pos >= chunk.len() && has_more_after {
                        return Ok(());
                    }
                    if in_consumed == 0 && out_consumed == 0 {
                        return Err(());
                    }
                }
                TINFLStatus::HasMoreOutput => {
                    if in_consumed == 0 && out_consumed == 0 {
                        return Err(());
                    }
                }
                _ => return Err(()),
            }
        }
    }
}

fn beam_search_fix_section(
    base: &[u8],
    expected_crc: u32,
    expected_uncomp_size: u64,
    lf_positions: &[usize],
    total_attempts: &mut u64,
) -> Option<Vec<usize>> {
    if try_decompress_check(base, expected_crc) {
        return Some(vec![]);
    }

    let n = lf_positions.len();
    eprintln!(
        "    beam search over {} LF positions (width={})",
        n, MAX_BEAM_WIDTH
    );

    let t = Instant::now();
    let mut candidates: Vec<BeamCandidate> = vec![BeamCandidate::new()];

    let mut feed_start = 0usize;

    for (i, &lf_pos) in lf_positions.iter().enumerate() {
        if lf_pos > feed_start {
            let chunk = &base[feed_start..lf_pos];
            candidates.retain_mut(|c| c.feed(chunk, true).is_ok());
            if candidates.is_empty() {
                eprintln!("    beam empty while feeding before LF #{}", i);
                return None;
            }
        }

        let lf_byte = base[lf_pos];
        let has_more = lf_pos + 1 < base.len() || (i + 1) < lf_positions.len();

        let mut new_cands: Vec<BeamCandidate> = Vec::with_capacity(candidates.len() * 2);
        for cand in &candidates {
            *total_attempts += 2;

            let mut a = cand.clone();
            if a.feed(&[lf_byte], has_more).is_ok() {
                new_cands.push(a);
            }

            let mut b = cand.clone();
            b.inserts.push(lf_pos);
            if b.feed(&[0x0D, lf_byte], has_more).is_ok() {
                new_cands.push(b);
            }
        }

        candidates = new_cands;
        if candidates.is_empty() {
            eprintln!("    beam empty after fork at LF #{}", i);
            return None;
        }

        if candidates.len() > MAX_BEAM_WIDTH {
            candidates.sort_by(|a, b| {
                a.inserts
                    .len()
                    .cmp(&b.inserts.len())
                    .then(b.total_out.cmp(&a.total_out))
            });
            candidates.truncate(MAX_BEAM_WIDTH);
        }

        if i > 0 && i % 500 == 0 {
            eprintln!(
                "    LF {}/{}: beam={}, t={:.2}s",
                i,
                n,
                candidates.len(),
                t.elapsed().as_secs_f64()
            );
        }

        feed_start = lf_pos + 1;
    }

    if feed_start < base.len() {
        let chunk = &base[feed_start..];
        candidates.retain_mut(|c| c.feed(chunk, false).is_ok());
    } else {
        candidates.retain_mut(|c| c.feed(&[], false).is_ok());
    }

    if candidates.is_empty() {
        eprintln!("    beam empty at final feed");
        return None;
    }

    let mut winners: Vec<BeamCandidate> = candidates
        .into_iter()
        .filter(|c| c.done && c.total_out == expected_uncomp_size)
        .collect();

    if winners.is_empty() {
        eprintln!("    no finished candidates");
        return None;
    }

    winners.sort_by_key(|c| c.inserts.len());
    for cand in winners {
        if cand.hasher.clone().finalize() == expected_crc {
            eprintln!(
                "    beam: {} CRs inserted, {:.2}s",
                cand.inserts.len(),
                t.elapsed().as_secs_f64()
            );
            return Some(cand.inserts);
        }
    }

    eprintln!("    beam: all finished candidates failed CRC");
    None
}

fn dfs_fix_section_fallback(
    base: &[u8],
    expected_crc: u32,
    lf_positions: &[usize],
    total_attempts: &mut u64,
) -> Option<Vec<usize>> {
    let t = Instant::now();
    let n = lf_positions.len();

    // keep_one: maintain `test` as `base` with a single 0x0D inserted at the
    // previous LF position. Transition to the next position by shifting the
    // affected slice by one byte in place — avoids a full O(base.len()) rewrite
    // on every iteration.
    let mut test = Vec::with_capacity(base.len() + 1);
    test.extend_from_slice(base);
    test.push(0);

    let mut prev_pos: Option<usize> = None;
    for &pos in lf_positions {
        *total_attempts += 1;
        match prev_pos {
            None => {
                test.copy_within(pos..base.len(), pos + 1);
                test[pos] = 0x0D;
            }
            Some(prev) => {
                test.copy_within(prev + 1..pos + 1, prev);
                test[pos] = 0x0D;
            }
        }
        if try_decompress_check(&test, expected_crc) {
            eprintln!("    dfs keep_one at {}, {:.2}s", pos, t.elapsed().as_secs_f64());
            return Some(vec![pos]);
        }
        prev_pos = Some(pos);
    }

    if n <= 2000 {
        let t2 = Instant::now();
        let mut test = vec![0u8; base.len() + 2];

        for i in 0..n {
            let p1 = lf_positions[i];
            // Reset `test` to `base` with a single insert at p1. The inner loop
            // will then mutate it incrementally without further full rewrites.
            test[..p1].copy_from_slice(&base[..p1]);
            test[p1] = 0x0D;
            test[p1 + 1..base.len() + 1].copy_from_slice(&base[p1..]);

            let mut prev_p2: Option<usize> = None;
            for j in (i + 1)..n {
                *total_attempts += 1;
                let p2 = lf_positions[j];
                match prev_p2 {
                    None => {
                        test.copy_within(p2 + 1..base.len() + 1, p2 + 2);
                        test[p2 + 1] = 0x0D;
                    }
                    Some(prev) => {
                        test.copy_within(prev + 2..p2 + 2, prev + 1);
                        test[p2 + 1] = 0x0D;
                    }
                }
                if try_decompress_check(&test, expected_crc) {
                    eprintln!(
                        "    dfs keep_two at {}+{}, {:.2}s",
                        p1, p2, t2.elapsed().as_secs_f64()
                    );
                    return Some(vec![p1, p2]);
                }
                prev_p2 = Some(p2);
            }
        }
    }

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

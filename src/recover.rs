use crate::zip_util;
use anyhow::{bail, Result};
use indicatif::{ProgressBar, ProgressStyle};
use memchr::memmem;
use miniz_oxide::inflate::core::{decompress, inflate_flags, DecompressorOxide};
use miniz_oxide::inflate::TINFLStatus;
use rand::seq::SliceRandom;
use rand::SeedableRng;
use rayon::prelude::*;
use std::time::Instant;

const BEAM_OUT_BUF_SIZE: usize = 32 * 1024;
const DEFAULT_MAX_BEAM_WIDTH: usize = 2000;

/// Beam width for stateful search. Overridable via `LOXAM_BEAM_WIDTH` env var
/// so we can expand capacity for hard cases without a rebuild. Each candidate
/// costs ~145 KiB (32 KiB LZ77 window + decoder state + hasher + validator),
/// so 10000 ≈ 1.5 GiB of working memory.
fn max_beam_width() -> usize {
    std::env::var("LOXAM_BEAM_WIDTH")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&w| w >= 1)
        .unwrap_or(DEFAULT_MAX_BEAM_WIDTH)
}

/// Known magic-byte prefixes for common file types. Returned as a static slice
/// of the raw bytes that MUST appear at offset 0 of the decoded output. Used
/// by beam search to kill trajectories whose decoded content is impossible
/// even if their decoder progression looks fine — a hard content oracle that
/// cuts the stored-block blind spot.
fn known_signature_for(filename: &str) -> &'static [u8] {
    let lower = filename.to_ascii_lowercase();
    if lower.ends_with(".png") {
        // PNG signature (8 bytes) + IHDR chunk header: length=13 (4 bytes) +
        // type="IHDR" (4 bytes). Both are deterministic for any valid PNG,
        // so extending the prefix from 8 to 16 bytes doubles the number of
        // LFs that participate in the early oracle check.
        &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A,
            0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52,
        ]
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        &[0xFF, 0xD8, 0xFF]
    } else if lower.ends_with(".pdf") {
        &[0x25, 0x50, 0x44, 0x46, 0x2D] // "%PDF-"
    } else if lower.ends_with(".gif") {
        &[0x47, 0x49, 0x46, 0x38] // "GIF8"
    } else if lower.ends_with(".zip") || lower.ends_with(".jar") {
        &[0x50, 0x4B, 0x03, 0x04]
    } else if lower.ends_with(".gz") {
        &[0x1F, 0x8B]
    } else if lower.ends_with(".bz2") {
        &[0x42, 0x5A, 0x68] // "BZh"
    } else if lower.ends_with(".7z") {
        &[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C]
    } else if lower.ends_with(".mp3") {
        &[0x49, 0x44, 0x33] // "ID3"
    } else if lower.ends_with(".mp4") || lower.ends_with(".mov") {
        // At offset 4, not offset 0, so harder; skip for now.
        &[]
    } else {
        &[]
    }
}

/// Streaming PNG chunk validator. Applied per-candidate during beam search:
/// as decoded bytes emit, `feed` advances through signature -> chunks -> IEND
/// and verifies every chunk's inline CRC32. A mismatch, an out-of-range
/// length, or a non-alphabetic chunk type kills the candidate immediately.
/// Because stored Deflate blocks emit decoded bytes verbatim, wrong CR/no-CR
/// choices within a stored block corrupt the decoded chunk content — and the
/// chunk CRC will not match. This is the strongest oracle we have against the
/// stored-block blind spot.
#[derive(Clone)]
enum PngPhase {
    Signature,
    ChunkLen,
    ChunkType,
    ChunkData,
    ChunkCrc,
    PostIend,
}

#[derive(Clone)]
struct PngValidator {
    phase: PngPhase,
    buf: [u8; 4],
    buf_len: usize,
    chunk_len: u32,
    chunk_type: [u8; 4],
    chunk_data_remaining: u32,
    crc: crc32fast::Hasher,
    sig_pos: usize,
}

impl PngValidator {
    fn new() -> Self {
        PngValidator {
            phase: PngPhase::Signature,
            buf: [0u8; 4],
            buf_len: 0,
            chunk_len: 0,
            chunk_type: [0u8; 4],
            chunk_data_remaining: 0,
            crc: crc32fast::Hasher::new(),
            sig_pos: 0,
        }
    }

    fn feed(&mut self, bytes: &[u8]) -> bool {
        let mut i = 0;
        while i < bytes.len() {
            // Fast path: inside ChunkData, consume as many bytes as possible
            // in one shot (CRC hasher SIMD, single subtract).
            if let PngPhase::ChunkData = self.phase {
                let avail = bytes.len() - i;
                let take = (self.chunk_data_remaining as usize).min(avail);
                if take > 0 {
                    self.crc.update(&bytes[i..i + take]);
                    self.chunk_data_remaining -= take as u32;
                    i += take;
                }
                if self.chunk_data_remaining == 0 {
                    self.phase = PngPhase::ChunkCrc;
                    self.buf_len = 0;
                }
                continue;
            }
            if !self.feed_byte(bytes[i]) {
                return false;
            }
            i += 1;
        }
        true
    }

    fn feed_byte(&mut self, b: u8) -> bool {
        match self.phase {
            PngPhase::Signature => {
                const SIG: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
                if b != SIG[self.sig_pos] {
                    return false;
                }
                self.sig_pos += 1;
                if self.sig_pos == 8 {
                    self.phase = PngPhase::ChunkLen;
                    self.buf_len = 0;
                }
            }
            PngPhase::ChunkLen => {
                self.buf[self.buf_len] = b;
                self.buf_len += 1;
                if self.buf_len == 4 {
                    self.chunk_len =
                        u32::from_be_bytes([self.buf[0], self.buf[1], self.buf[2], self.buf[3]]);
                    // PNG spec: chunk length must not exceed 2^31 - 1.
                    if self.chunk_len > (1u32 << 31) - 1 {
                        return false;
                    }
                    self.chunk_data_remaining = self.chunk_len;
                    self.crc = crc32fast::Hasher::new();
                    self.phase = PngPhase::ChunkType;
                    self.buf_len = 0;
                }
            }
            PngPhase::ChunkType => {
                if !((b >= 0x41 && b <= 0x5A) || (b >= 0x61 && b <= 0x7A)) {
                    return false;
                }
                self.chunk_type[self.buf_len] = b;
                self.crc.update(&[b]);
                self.buf_len += 1;
                if self.buf_len == 4 {
                    if self.chunk_data_remaining == 0 {
                        self.phase = PngPhase::ChunkCrc;
                        self.buf_len = 0;
                    } else {
                        self.phase = PngPhase::ChunkData;
                    }
                }
            }
            PngPhase::ChunkData => {
                self.crc.update(&[b]);
                self.chunk_data_remaining -= 1;
                if self.chunk_data_remaining == 0 {
                    self.phase = PngPhase::ChunkCrc;
                    self.buf_len = 0;
                }
            }
            PngPhase::ChunkCrc => {
                self.buf[self.buf_len] = b;
                self.buf_len += 1;
                if self.buf_len == 4 {
                    let expected = u32::from_be_bytes([
                        self.buf[0], self.buf[1], self.buf[2], self.buf[3],
                    ]);
                    let actual = self.crc.clone().finalize();
                    if expected != actual {
                        return false;
                    }
                    if &self.chunk_type == b"IEND" {
                        self.phase = PngPhase::PostIend;
                    } else {
                        self.phase = PngPhase::ChunkLen;
                    }
                    self.buf_len = 0;
                }
            }
            PngPhase::PostIend => {
                return false;
            }
        }
        true
    }
}

/// Pick a streaming content validator for a known file type. Returns None if
/// we have no structural parser for this type (beam search falls back to the
/// prefix/trailer byte oracles only).
fn new_validator_for(filename: &str) -> Option<PngValidator> {
    let lower = filename.to_ascii_lowercase();
    if lower.ends_with(".png") {
        Some(PngValidator::new())
    } else {
        None
    }
}

/// Known final bytes for file types whose trailer is fixed. Applied once, at
/// the end of beam search, to filter winners before CRC check.
fn known_trailer_for(filename: &str) -> &'static [u8] {
    let lower = filename.to_ascii_lowercase();
    if lower.ends_with(".png") {
        // PNG IEND chunk: 4-byte length=0, "IEND", 4-byte CRC32 of "IEND"
        &[0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82]
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        &[0xFF, 0xD9] // JPEG end-of-image marker
    } else if lower.ends_with(".gif") {
        &[0x3B] // GIF trailer byte
    } else {
        &[]
    }
}

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
            let expected_prefix = known_signature_for(&entry.name);
            let expected_trailer = known_trailer_for(&entry.name);
            let validator = new_validator_for(&entry.name);
            if !expected_prefix.is_empty() || !expected_trailer.is_empty() || validator.is_some() {
                eprintln!(
                    "    content oracles: prefix={} bytes, trailer={} bytes, streaming_validator={}",
                    expected_prefix.len(),
                    expected_trailer.len(),
                    validator.is_some()
                );
            }
            let result = beam_search_fix_section(
                compressed_data,
                entry.crc32_expected,
                entry.uncompressed_size,
                &lf_positions,
                expected_prefix,
                expected_trailer,
                validator,
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
    /// `true` once the candidate's first `expected_prefix.len()` decoded bytes
    /// have been verified to match the known file signature. Defaults to
    /// `true` when there is no signature to check.
    prefix_ok: bool,
    /// Optional streaming content validator. When present, every emitted byte
    /// is fed to it; a validator-reported violation kills the candidate.
    validator: Option<PngValidator>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FeedErr {
    /// Decompressor said Done but chunk still had unconsumed input, or chunk
    /// was fed to an already-done candidate.
    UnexpectedDone,
    /// Decompressor returned a fatal status (FailedCannotMakeProgress, BadParam,
    /// Adler32Mismatch, or a negative error code).
    BadStatus(TINFLStatus),
    /// Decompressor made no progress on either side (neither input consumed nor
    /// output produced) — stream is malformed at this position.
    Stuck,
    /// Streaming content validator rejected the emitted bytes (bad signature,
    /// bad chunk structure, or bad chunk CRC).
    ContentInvalid,
}

impl BeamCandidate {
    fn new(has_signature: bool, validator: Option<PngValidator>) -> Self {
        BeamCandidate {
            state: Box::new(DecompressorOxide::new()),
            hasher: crc32fast::Hasher::new(),
            out_buf: vec![0u8; BEAM_OUT_BUF_SIZE],
            out_pos: 0,
            total_out: 0,
            inserts: Vec::new(),
            done: false,
            prefix_ok: !has_signature,
            validator,
        }
    }

    /// Returns true if the candidate should be kept under the signature oracle.
    /// - If no signature configured (`prefix_ok` already true): keep.
    /// - If not enough bytes decoded yet to cover the prefix: keep (too early
    ///   to tell).
    /// - If the LZ77 ring buffer has already wrapped past the prefix region
    ///   without us validating first, we missed the window — trust the
    ///   candidate (should not normally happen since we validate every LF).
    /// - Otherwise compare out_buf[0..prefix.len()] against the signature.
    fn check_prefix(&mut self, expected_prefix: &[u8]) -> bool {
        if self.prefix_ok {
            return true;
        }
        if expected_prefix.is_empty() {
            self.prefix_ok = true;
            return true;
        }
        let need = expected_prefix.len() as u64;
        if self.total_out < need {
            return true;
        }
        if self.total_out > self.out_buf.len() as u64 {
            // Prefix region already overwritten in ring buffer — we missed it.
            self.prefix_ok = true;
            return true;
        }
        let matches = &self.out_buf[..expected_prefix.len()] == expected_prefix;
        if matches {
            self.prefix_ok = true;
        }
        matches
    }

    /// Compare the last `trailer.len()` emitted bytes against `trailer`. Bytes
    /// live in a ring buffer at `(out_pos - k) % buf_len` for k = 1..=len.
    /// Used once, after the final feed, to filter winners before CRC — cheap
    /// and kills any candidate whose stream ended with wrong content.
    fn ends_with_trailer(&self, trailer: &[u8]) -> bool {
        if trailer.is_empty() {
            return true;
        }
        let n = trailer.len();
        if (self.total_out as usize) < n {
            return false;
        }
        let buf_len = self.out_buf.len();
        if n > buf_len {
            return true; // trailer larger than window — can't check reliably
        }
        for (i, &expected) in trailer.iter().enumerate() {
            let offset_from_end = n - 1 - i;
            let pos = (self.out_pos + buf_len - 1 - offset_from_end) % buf_len;
            if self.out_buf[pos] != expected {
                return false;
            }
        }
        true
    }

    fn feed(&mut self, chunk: &[u8], has_more_after: bool) -> Result<(), FeedErr> {
        if self.done {
            return if chunk.is_empty() {
                Ok(())
            } else {
                Err(FeedErr::UnexpectedDone)
            };
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
                    if let Some(v) = &mut self.validator {
                        if !v.feed(&self.out_buf[start..end]) {
                            return Err(FeedErr::ContentInvalid);
                        }
                    }
                } else {
                    let wrap_end = end & (buf_len - 1);
                    self.hasher.update(&self.out_buf[start..]);
                    self.hasher.update(&self.out_buf[..wrap_end]);
                    if let Some(v) = &mut self.validator {
                        if !v.feed(&self.out_buf[start..]) {
                            return Err(FeedErr::ContentInvalid);
                        }
                        if !v.feed(&self.out_buf[..wrap_end]) {
                            return Err(FeedErr::ContentInvalid);
                        }
                    }
                }
                self.out_pos = end & (buf_len - 1);
                self.total_out += out_consumed as u64;
            }

            match status {
                TINFLStatus::Done => {
                    self.done = true;
                    // Align with miniz_oxide::inflate::decompress_to_vec (used
                    // by DFS's try_decompress_check): at terminal feed we
                    // tolerate leftover bytes in the chunk, because a
                    // well-formed stream's BFINAL bit can land before the
                    // byte-aligned end of the compressed region. Mid-stream
                    // Done-with-leftover remains an error (it means our
                    // inserts made the decoder terminate too early).
                    if in_pos < chunk.len() && has_more_after {
                        return Err(FeedErr::UnexpectedDone);
                    }
                    return Ok(());
                }
                TINFLStatus::NeedsMoreInput => {
                    if in_pos >= chunk.len() && has_more_after {
                        return Ok(());
                    }
                    if in_consumed == 0 && out_consumed == 0 {
                        return Err(FeedErr::Stuck);
                    }
                }
                TINFLStatus::HasMoreOutput => {
                    if in_consumed == 0 && out_consumed == 0 {
                        return Err(FeedErr::Stuck);
                    }
                }
                other => return Err(FeedErr::BadStatus(other)),
            }
        }
    }
}

fn beam_search_fix_section(
    base: &[u8],
    expected_crc: u32,
    expected_uncomp_size: u64,
    lf_positions: &[usize],
    expected_prefix: &[u8],
    expected_trailer: &[u8],
    initial_validator: Option<PngValidator>,
    total_attempts: &mut u64,
) -> Option<Vec<usize>> {
    if try_decompress_check(base, expected_crc) {
        return Some(vec![]);
    }

    let n = lf_positions.len();
    let beam_cap = max_beam_width();
    eprintln!(
        "    beam search over {} LF positions (width={})",
        n, beam_cap
    );

    let t = Instant::now();
    let has_signature = !expected_prefix.is_empty();
    let mut candidates: Vec<BeamCandidate> =
        vec![BeamCandidate::new(has_signature, initial_validator)];
    let mut prefix_kills: u64 = 0;

    let mut feed_start = 0usize;

    // Per-LF branching counters accumulated since the last progress log,
    // so we can see survival asymmetry between "no insert" and "insert CR"
    // forks — if the correct trajectory is being evicted, the asymmetry often
    // shifts dramatically.
    let mut fork_a_kept = 0u64;
    let mut fork_a_killed = 0u64;
    let mut fork_b_kept = 0u64;
    let mut fork_b_killed = 0u64;
    let mut content_kills: u64 = 0;

    for (i, &lf_pos) in lf_positions.iter().enumerate() {
        if lf_pos > feed_start {
            let chunk = &base[feed_start..lf_pos];
            let before = candidates.len();
            candidates.retain_mut(|c| match c.feed(chunk, true) {
                Ok(()) => true,
                Err(FeedErr::ContentInvalid) => {
                    content_kills += 1;
                    false
                }
                Err(_) => false,
            });
            if candidates.is_empty() {
                eprintln!(
                    "    beam empty while feeding before LF #{} (had {} candidates; chunk_len={})",
                    i, before, chunk.len()
                );
                return None;
            }
        }

        let lf_byte = base[lf_pos];
        let has_more = lf_pos + 1 < base.len() || (i + 1) < lf_positions.len();

        let mut new_cands: Vec<BeamCandidate> = Vec::with_capacity(candidates.len() * 2);
        for cand in &candidates {
            *total_attempts += 2;

            let mut a = cand.clone();
            match a.feed(&[lf_byte], has_more) {
                Ok(()) => {
                    new_cands.push(a);
                    fork_a_kept += 1;
                }
                Err(FeedErr::ContentInvalid) => {
                    fork_a_killed += 1;
                    content_kills += 1;
                }
                Err(_) => fork_a_killed += 1,
            }

            let mut b = cand.clone();
            b.inserts.push(lf_pos);
            match b.feed(&[0x0D, lf_byte], has_more) {
                Ok(()) => {
                    new_cands.push(b);
                    fork_b_kept += 1;
                }
                Err(FeedErr::ContentInvalid) => {
                    fork_b_killed += 1;
                    content_kills += 1;
                }
                Err(_) => fork_b_killed += 1,
            }
        }

        candidates = new_cands;
        if candidates.is_empty() {
            eprintln!(
                "    beam empty after fork at LF #{} (fork_a: {}/{}, fork_b: {}/{})",
                i,
                fork_a_kept,
                fork_a_kept + fork_a_killed,
                fork_b_kept,
                fork_b_kept + fork_b_killed
            );
            return None;
        }

        // Signature oracle: kill candidates whose first prefix bytes diverge
        // from the known file magic. This cuts the stored-block blind spot
        // because stored blocks emit decoded bytes verbatim — a wrong
        // insertion within the signature region produces wrong output bytes
        // that we can directly compare against the known prefix.
        if has_signature {
            let before = candidates.len();
            candidates.retain_mut(|c| c.check_prefix(expected_prefix));
            let killed = before - candidates.len();
            prefix_kills += killed as u64;
            if candidates.is_empty() {
                eprintln!(
                    "    beam empty after prefix check at LF #{} (prefix killed {} total so far)",
                    i, prefix_kills
                );
                return None;
            }
        }

        if candidates.len() > beam_cap {
            // Water-fill bucketed pruning: equal initial quota per insert-count
            // bucket, then redistribute unused slots from under-quota buckets
            // to over-quota ones (sorted smallest bucket first). Within a
            // bucket that must be truncated, shuffle deterministically so the
            // correct trajectory isn't evicted by tied total_out and stable
            // insertion order — every candidate in the bucket gets equal
            // probability of surviving a cut. Tied total_out is the common
            // case inside stored Deflate blocks, where 100+ candidates can
            // share identical decode progress.
            let mut by_bucket: std::collections::BTreeMap<usize, Vec<BeamCandidate>> =
                Default::default();
            for cand in candidates.drain(..) {
                by_bucket.entry(cand.inserts.len()).or_default().push(cand);
            }

            let mut sorted_buckets: Vec<(usize, Vec<BeamCandidate>)> =
                by_bucket.into_iter().collect();
            // Smallest buckets first — they get absorbed fully, freeing budget.
            sorted_buckets.sort_by_key(|(_, v)| v.len());

            let mut selected: Vec<BeamCandidate> = Vec::with_capacity(beam_cap);
            let mut remaining_budget = beam_cap;
            let total_buckets = sorted_buckets.len();

            // Deterministic RNG seeded by iteration so results are reproducible
            // across runs but differ between iterations (avoids always
            // evicting the same candidate position).
            let mut rng = rand::rngs::StdRng::seed_from_u64(0xC0DE_F00D ^ (i as u64));

            for (idx, (_, mut bucket)) in sorted_buckets.into_iter().enumerate() {
                let buckets_left = total_buckets - idx;
                let per = (remaining_budget / buckets_left).max(1);
                let take = bucket.len().min(per);

                if bucket.len() > take {
                    // Partial-shuffle the first `take` slots uniformly among
                    // all candidates, then truncate. This samples without
                    // replacement from the bucket with uniform probability —
                    // O(bucket.len()) time without a full sort.
                    bucket.partial_shuffle(&mut rng, take);
                    bucket.truncate(take);
                }
                remaining_budget = remaining_budget.saturating_sub(bucket.len());
                selected.append(&mut bucket);
            }

            candidates = selected;
        }

        let log_this = (i > 0 && i % 500 == 0) || (i + 1 == n);
        if log_this {
            // inserts.len() distribution across the beam — min/median/max and
            // top 5 most-populous buckets.
            let mut lens: Vec<usize> = candidates.iter().map(|c| c.inserts.len()).collect();
            lens.sort_unstable();
            let lmin = lens.first().copied().unwrap_or(0);
            let lmax = lens.last().copied().unwrap_or(0);
            let lmed = lens.get(lens.len() / 2).copied().unwrap_or(0);

            let mut bucket_counts: Vec<(usize, usize)> = Vec::new();
            let mut cur_key = usize::MAX;
            for &l in &lens {
                if l != cur_key {
                    bucket_counts.push((l, 1));
                    cur_key = l;
                } else {
                    let last = bucket_counts.last_mut().unwrap();
                    last.1 += 1;
                }
            }
            bucket_counts.sort_by(|a, b| b.1.cmp(&a.1));
            let top: Vec<String> = bucket_counts
                .iter()
                .take(5)
                .map(|(k, v)| format!("{}x{}", k, v))
                .collect();

            // total_out distribution — how far decoded
            let mut outs: Vec<u64> = candidates.iter().map(|c| c.total_out).collect();
            outs.sort_unstable();
            let omin = outs.first().copied().unwrap_or(0);
            let omax = outs.last().copied().unwrap_or(0);
            let omed = outs.get(outs.len() / 2).copied().unwrap_or(0);

            let done_count = candidates.iter().filter(|c| c.done).count();

            eprintln!(
                "    LF {}/{}: beam={}, t={:.2}s | inserts [{},{},{}] buckets={} top=[{}] | out [{},{},{}]/{} done={} | forks a:{}+/{}- b:{}+/{}- | sig_kills={} content_kills={}",
                i,
                n,
                candidates.len(),
                t.elapsed().as_secs_f64(),
                lmin, lmed, lmax,
                bucket_counts.len(),
                top.join(","),
                omin, omed, omax,
                expected_uncomp_size,
                done_count,
                fork_a_kept, fork_a_killed,
                fork_b_kept, fork_b_killed,
                prefix_kills,
                content_kills
            );
            fork_a_kept = 0;
            fork_a_killed = 0;
            fork_b_kept = 0;
            fork_b_killed = 0;
            prefix_kills = 0;
            content_kills = 0;
        }

        feed_start = lf_pos + 1;
    }

    let before_final = candidates.len();
    let final_chunk_len = if feed_start < base.len() {
        base.len() - feed_start
    } else {
        0
    };

    // Final feed: track failure reason for every candidate so we understand
    // *why* the stream couldn't terminate cleanly.
    let mut survived: Vec<BeamCandidate> = Vec::new();
    let mut err_unexpected_done = 0u64;
    let mut err_stuck = 0u64;
    let mut err_content = 0u64;
    let mut err_bad_status: std::collections::BTreeMap<String, u64> = Default::default();

    let final_chunk: Vec<u8> = if feed_start < base.len() {
        base[feed_start..].to_vec()
    } else {
        Vec::new()
    };

    for mut c in candidates.drain(..) {
        match c.feed(&final_chunk, false) {
            Ok(()) => survived.push(c),
            Err(FeedErr::UnexpectedDone) => err_unexpected_done += 1,
            Err(FeedErr::Stuck) => err_stuck += 1,
            Err(FeedErr::ContentInvalid) => err_content += 1,
            Err(FeedErr::BadStatus(s)) => {
                *err_bad_status.entry(format!("{:?}", s)).or_insert(0) += 1;
            }
        }
    }

    eprintln!(
        "    final feed: {} -> {} survived (chunk_len={}). errors: UnexpectedDone={}, Stuck={}, ContentInvalid={}, BadStatus={:?}",
        before_final,
        survived.len(),
        final_chunk_len,
        err_unexpected_done,
        err_stuck,
        err_content,
        err_bad_status
    );

    if survived.is_empty() {
        eprintln!("    beam empty at final feed");
        return None;
    }

    // Categorise survivors: done vs not done, total_out vs expected.
    let done_total = survived.iter().filter(|c| c.done).count();
    let size_match = survived
        .iter()
        .filter(|c| c.total_out == expected_uncomp_size)
        .count();
    let done_and_size = survived
        .iter()
        .filter(|c| c.done && c.total_out == expected_uncomp_size)
        .count();
    let mut outs: Vec<u64> = survived.iter().map(|c| c.total_out).collect();
    outs.sort_unstable();
    let omin = outs.first().copied().unwrap_or(0);
    let omax = outs.last().copied().unwrap_or(0);
    let omed = outs.get(outs.len() / 2).copied().unwrap_or(0);

    eprintln!(
        "    survivors: done={}, size_match={}, done+size={}, total_out range=[{}, median {}, {}]/{}",
        done_total, size_match, done_and_size, omin, omed, omax, expected_uncomp_size
    );

    let mut winners: Vec<BeamCandidate> = survived
        .into_iter()
        .filter(|c| c.done && c.total_out == expected_uncomp_size)
        .collect();

    if winners.is_empty() {
        eprintln!(
            "    no finished candidates (need both done=true and total_out={})",
            expected_uncomp_size
        );
        return None;
    }

    if !expected_trailer.is_empty() {
        let before = winners.len();
        winners.retain(|c| c.ends_with_trailer(expected_trailer));
        eprintln!(
            "    trailer oracle: {} -> {} winners (trailer={} bytes)",
            before,
            winners.len(),
            expected_trailer.len()
        );
        if winners.is_empty() {
            eprintln!("    beam: no candidates match known trailer");
            return None;
        }
    }

    winners.sort_by_key(|c| c.inserts.len());
    let mut crc_fail = 0u64;
    for cand in winners {
        let got_crc = cand.hasher.clone().finalize();
        if got_crc == expected_crc {
            eprintln!(
                "    beam: {} CRs inserted, {:.2}s",
                cand.inserts.len(),
                t.elapsed().as_secs_f64()
            );
            return Some(cand.inserts);
        }
        crc_fail += 1;
    }

    eprintln!(
        "    beam: all {} finished candidates failed CRC (expected {:08X})",
        crc_fail, expected_crc
    );
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds the bytes of a real PNG file through PngValidator in various
    /// chunk boundaries. The validator must never reject a valid PNG.
    #[test]
    fn png_validator_accepts_real_png() {
        let path = r"C:\Users\Dmytro\loxam\.eck\files\Scan_20260201.png";
        let data = std::fs::read(path).expect("test PNG missing");

        // One-shot feed
        let mut v = PngValidator::new();
        assert!(v.feed(&data), "validator rejected full-PNG single feed");

        // Feed byte-by-byte (stresses all phase transitions)
        let mut v = PngValidator::new();
        for (i, &b) in data.iter().enumerate() {
            assert!(v.feed(&[b]), "byte-by-byte rejection at index {}", i);
        }

        // Feed with awkward chunk sizes (13 bytes at a time)
        let mut v = PngValidator::new();
        for chunk in data.chunks(13) {
            assert!(v.feed(chunk), "chunked feed rejection");
        }

        // Feed with random-like chunks
        let mut v = PngValidator::new();
        let mut i = 0;
        let sizes = [1, 2, 7, 37, 128, 1, 3, 4096, 7, 15];
        let mut si = 0;
        while i < data.len() {
            let take = sizes[si % sizes.len()].min(data.len() - i);
            assert!(v.feed(&data[i..i + take]), "random-chunk rejection at {}", i);
            i += take;
            si += 1;
        }
    }
}

use anyhow::{bail, Result};
use crc32fast::Hasher;
use memchr::memchr_iter;

struct LocalHeaderInfo {
    offset: usize,
    compressed_size: u64,
    uncompressed_size: u64,
    crc32: u32,
}

pub fn crc32(data: &[u8]) -> u32 {
    let mut hasher = Hasher::new();
    hasher.update(data);
    hasher.finalize()
}

fn deflate_compress(data: &[u8]) -> Vec<u8> {
    miniz_oxide::deflate::compress_to_vec(data, 6)
}

pub fn create_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut buf = Vec::new();
    let mut central_dir = Vec::new();
    let mut local_headers: Vec<LocalHeaderInfo> = Vec::new();

    for (name, data) in files {
        let name_bytes = name.as_bytes();
        let crc = crc32(data);
        let compressed = deflate_compress(data);
        let offset = buf.len() as u32;

        write_local_header(
            &mut buf,
            name_bytes,
            &compressed,
            crc,
            data.len() as u32,
        );

        local_headers.push(LocalHeaderInfo {
            offset: offset as usize,
            compressed_size: compressed.len() as u64,
            uncompressed_size: data.len() as u64,
            crc32: crc,
        });
    }

    let central_dir_offset = buf.len() as u32;

    for (i, (name, _)) in files.iter().enumerate() {
        let h = &local_headers[i];
        write_central_dir_entry(
            &mut central_dir,
            name.as_bytes(),
            h.offset as u32,
            h.compressed_size as u32,
            h.uncompressed_size as u32,
            h.crc32,
        );
    }

    buf.extend_from_slice(&central_dir);
    let central_dir_size = central_dir.len() as u32;

    write_eocd(
        &mut buf,
        files.len() as u16,
        central_dir_offset,
        central_dir_size,
    );

    buf
}

fn write_local_header(
    buf: &mut Vec<u8>,
    filename: &[u8],
    compressed_data: &[u8],
    crc32: u32,
    uncompressed_size: u32,
) {
    buf.extend_from_slice(&0x04034b50u32.to_le_bytes());
    buf.extend_from_slice(&20u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&8u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&crc32.to_le_bytes());
    buf.extend_from_slice(&(compressed_data.len() as u32).to_le_bytes());
    buf.extend_from_slice(&uncompressed_size.to_le_bytes());
    buf.extend_from_slice(&(filename.len() as u16).to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(filename);
    buf.extend_from_slice(compressed_data);
}

fn write_central_dir_entry(
    buf: &mut Vec<u8>,
    filename: &[u8],
    local_header_offset: u32,
    compressed_size: u32,
    uncompressed_size: u32,
    crc32_val: u32,
) {
    buf.extend_from_slice(&0x02014b50u32.to_le_bytes());
    buf.extend_from_slice(&20u16.to_le_bytes());
    buf.extend_from_slice(&20u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&8u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&crc32_val.to_le_bytes());
    buf.extend_from_slice(&compressed_size.to_le_bytes());
    buf.extend_from_slice(&uncompressed_size.to_le_bytes());
    buf.extend_from_slice(&(filename.len() as u16).to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u32.to_le_bytes());
    buf.extend_from_slice(&local_header_offset.to_le_bytes());
    buf.extend_from_slice(filename);
}

fn write_eocd(
    buf: &mut Vec<u8>,
    num_entries: u16,
    central_dir_offset: u32,
    central_dir_size: u32,
) {
    buf.extend_from_slice(&0x06054b50u32.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
    buf.extend_from_slice(&num_entries.to_le_bytes());
    buf.extend_from_slice(&num_entries.to_le_bytes());
    buf.extend_from_slice(&central_dir_size.to_le_bytes());
    buf.extend_from_slice(&central_dir_offset.to_le_bytes());
    buf.extend_from_slice(&0u16.to_le_bytes());
}

pub struct ParsedZip {
    pub entries: Vec<ParsedEntry>,
}

pub struct ParsedEntry {
    pub name: String,
    pub crc32_expected: u32,
    pub crc32_actual: Option<u32>,
    pub compressed_size: u64,
    pub uncompressed_size: u64,
}

fn parse_zip64_sizes(
    extra: &[u8],
    need_uncomp: bool,
    need_comp: bool,
) -> (Option<u64>, Option<u64>) {
    let mut pos = 0;
    while pos + 4 <= extra.len() {
        let id = u16::from_le_bytes(extra[pos..pos + 2].try_into().unwrap());
        let size = u16::from_le_bytes(extra[pos + 2..pos + 4].try_into().unwrap()) as usize;
        if id == 0x0001 {
            let data_start = pos + 4;
            let data_end = data_start + size;
            if data_end <= extra.len() {
                let mut dp = data_start;
                let mut uncomp = None;
                let mut comp = None;
                if need_uncomp && dp + 8 <= data_end {
                    uncomp = Some(u64::from_le_bytes(extra[dp..dp + 8].try_into().unwrap()));
                    dp += 8;
                }
                if need_comp && dp + 8 <= data_end {
                    comp = Some(u64::from_le_bytes(extra[dp..dp + 8].try_into().unwrap()));
                }
                return (uncomp, comp);
            }
        }
        pos += 4 + size;
    }
    (None, None)
}

pub fn parse_and_validate(data: &[u8]) -> Result<ParsedZip> {
    let mut pos = 0;
    let mut entries = Vec::new();

    while pos + 4 <= data.len() {
        let sig = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
        if sig != 0x04034b50 {
            break;
        }
        if pos + 30 > data.len() {
            bail!("Local header truncated at offset {}", pos);
        }

        let method = u16::from_le_bytes(data[pos + 8..pos + 10].try_into().unwrap());
        let crc32_val = u32::from_le_bytes(data[pos + 14..pos + 18].try_into().unwrap());
        let comp_size_raw = u32::from_le_bytes(data[pos + 18..pos + 22].try_into().unwrap());
        let uncomp_size_raw = u32::from_le_bytes(data[pos + 22..pos + 26].try_into().unwrap());
        let name_len = u16::from_le_bytes(data[pos + 26..pos + 28].try_into().unwrap()) as usize;
        let extra_len = u16::from_le_bytes(data[pos + 28..pos + 30].try_into().unwrap()) as usize;

        let header_end = pos + 30 + name_len + extra_len;
        if header_end > data.len() {
            bail!("Header extends beyond data at offset {}", pos);
        }

        let name = String::from_utf8_lossy(&data[pos + 30..pos + 30 + name_len]).to_string();

        let extra_data = if extra_len > 0 && pos + 30 + name_len + extra_len <= data.len() {
            &data[pos + 30 + name_len..header_end]
        } else {
            &[]
        };

        let (comp_size, uncomp_size): (u64, u64) =
            if comp_size_raw == 0xFFFFFFFF || uncomp_size_raw == 0xFFFFFFFF {
                let (u64_val, c64_val) = parse_zip64_sizes(
                    extra_data,
                    uncomp_size_raw == 0xFFFFFFFF,
                    comp_size_raw == 0xFFFFFFFF,
                );
                (
                    c64_val.unwrap_or(comp_size_raw as u64),
                    u64_val.unwrap_or(uncomp_size_raw as u64),
                )
            } else {
                (comp_size_raw as u64, uncomp_size_raw as u64)
            };

        let data_start = header_end;
        let data_end = data_start + comp_size as usize;

        if data_end > data.len() {
            bail!(
                "Compressed data extends beyond file for '{}' (need {} bytes from offset {}, have {})",
                name, comp_size, data_start, data.len()
            );
        }

        let compressed_slice = &data[data_start..data_end];

        let crc32_actual = if method == 8 {
            try_decompress(compressed_slice).ok().map(|d| crc32(&d))
        } else if method == 0 {
            Some(crc32(compressed_slice))
        } else {
            None
        };

        entries.push(ParsedEntry {
            name,
            crc32_expected: crc32_val,
            crc32_actual,
            compressed_size: comp_size,
            uncompressed_size: uncomp_size,
        });

        pos = data_end;
    }

    Ok(ParsedZip { entries })
}

fn try_decompress(data: &[u8]) -> Result<Vec<u8>> {
    miniz_oxide::inflate::decompress_to_vec(data)
        .map_err(|e| anyhow::anyhow!("Decompression failed: {:?}", e))
}

pub fn find_crlf_positions(data: &[u8]) -> Vec<usize> {
    let mut positions = Vec::new();
    for pos in memchr_iter(0x0D, data) {
        if pos + 1 < data.len() && data[pos + 1] == 0x0A {
            positions.push(pos);
        }
    }
    positions
}

pub fn is_valid_zip_signature(data: &[u8]) -> bool {
    data.len() >= 4 && u32::from_le_bytes(data[0..4].try_into().unwrap()) == 0x04034b50
}

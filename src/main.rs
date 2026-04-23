mod corrupt;
mod recover;
mod zip_util;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use rand::Rng;

#[derive(Parser)]
#[command(name = "loxam")]
#[command(about = "ZIP recovery tool for \\n→\\r\\n corruption")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Test {
        #[arg(long, default_values_t = vec![50, 80, 120])]
        sizes: Vec<usize>,
    },
    Stress {
        #[arg(long, default_value = "100")]
        runs: usize,
        #[arg(long, default_value = "500")]
        size: usize,
    },
    Corrupt {
        input: String,
        output: String,
    },
    Recover {
        input: String,
        output: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Test { sizes } => run_test(&sizes),
        Commands::Stress { runs, size } => run_stress(runs, size),
        Commands::Corrupt { input, output } => run_corrupt(&input, &output),
        Commands::Recover { input, output } => run_recover(&input, &output),
    }
}

fn random_text(rng: &mut impl Rng, size: usize) -> Vec<u8> {
    let chars = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 .,!?;:'\"()-\n\r\t";
    let mut result = Vec::with_capacity(size);
    for _ in 0..size {
        result.push(chars[rng.gen_range(0..chars.len())]);
    }
    result
}

fn run_test(sizes: &[usize]) -> Result<()> {
    let mut rng = rand::thread_rng();

    println!("=== Loxam ZIP Recovery Test ===\n");

    let file_a = random_text(&mut rng, sizes.get(0).copied().unwrap_or(50));
    let file_b = random_text(&mut rng, sizes.get(1).copied().unwrap_or(80));
    let file_c = random_text(&mut rng, sizes.get(2).copied().unwrap_or(120));

    let files: Vec<(&str, &[u8])> = vec![
        ("file_a.txt", &file_a),
        ("file_b.txt", &file_b),
        ("file_c.txt", &file_c),
    ];

    println!("Generated {} test files:", files.len());
    for (name, data) in &files {
        println!("  {} - {} bytes", name, data.len());
    }

    let original_zip = zip_util::create_zip(&files);
    println!("\nOriginal ZIP: {} bytes", original_zip.len());

    let standalone_lf = corrupt::find_standalone_lf(&original_zip);
    println!("Standalone LF (0x0A) in original: {} positions", standalone_lf.len());

    let natural_crlf = count_natural_crlf(&original_zip);
    println!("Natural CRLF (0x0D 0x0A) in original: {} positions", natural_crlf);

    let corrupted = corrupt::corrupt(&original_zip);
    println!("\nCorrupted:   {} bytes (+{})", corrupted.len(), corrupted.len() - original_zip.len());

    let crlf_in_corrupted = zip_util::find_crlf_positions(&corrupted);
    println!("CRLF positions in corrupted: {}", crlf_in_corrupted.len());

    println!("\n--- Recovery attempt ---");
    let result = recover::recover(&corrupted).context("Recovery failed")?;

    println!("\n=== SUCCESS ===");
    println!("Strategy: {}", result.strategy);
    println!("Attempts: {}", result.attempts);
    println!("Recovered size: {} bytes (original: {})", result.data.len(), original_zip.len());

    if result.data == original_zip {
        println!("PERFECT MATCH: recovered data is identical to original!");
    } else {
        println!("WARNING: recovered data differs from original!");
        let diffs = count_diffs(&result.data, &original_zip);
        println!("  {} bytes differ", diffs);
    }

    print_extracted_files(&result.data)?;
    Ok(())
}

fn run_stress(runs: usize, size: usize) -> Result<()> {
    println!("=== Stress Test: {} runs, file size {} ===\n", runs, size);
    let mut rng = rand::thread_rng();

    let mut ok = 0usize;
    let mut fail = 0usize;
    let mut perfect = 0usize;
    let mut natural_crlf_total = 0usize;
    let mut strategy_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for run in 0..runs {
        let file_a = random_text(&mut rng, size);
        let file_b = random_text(&mut rng, size / 2);
        let file_c = random_text(&mut rng, size * 2);

        let files: Vec<(&str, &[u8])> = vec![
            ("a.txt", &file_a),
            ("b.txt", &file_b),
            ("c.txt", &file_c),
        ];

        let original_zip = zip_util::create_zip(&files);
        let natural_crlf = count_natural_crlf(&original_zip);
        natural_crlf_total += natural_crlf;

        let corrupted = corrupt::corrupt(&original_zip);

        match recover::recover(&corrupted) {
            Ok(result) => {
                *strategy_counts.entry(result.strategy.clone()).or_insert(0) += 1;
                if result.data == original_zip {
                    ok += 1;
                    perfect += 1;
                } else if validate_partial(&result.data) {
                    ok += 1;
                } else {
                    fail += 1;
                    println!("Run {}: recovery produced invalid ZIP (natural CRLFs: {})", run + 1, natural_crlf);
                }
            }
            Err(e) => {
                fail += 1;
                println!("Run {}: FAILED - {} (natural CRLFs: {})", run + 1, e, natural_crlf);
            }
        }

        if (run + 1) % 10 == 0 {
            println!(
                "Progress: {}/{} (ok={}, fail={}, natural_crlf_avg={:.1})",
                run + 1, runs, ok, fail, natural_crlf_total as f64 / (run + 1) as f64
            );
        }
    }

    println!("\n=== Results ===");
    println!("Total: {} | Perfect: {} | OK: {} | Failed: {}", runs, perfect, ok, fail);
    println!("Average natural CRLFs: {:.2}", natural_crlf_total as f64 / runs as f64);
    println!("Strategies used: {:?}", strategy_counts);

    if fail > 0 {
        anyhow::bail!("{} out of {} runs failed", fail, runs);
    }
    Ok(())
}

fn count_natural_crlf(data: &[u8]) -> usize {
    let mut count = 0;
    for i in 0..data.len().saturating_sub(1) {
        if data[i] == 0x0D && data[i + 1] == 0x0A {
            count += 1;
        }
    }
    count
}

fn count_diffs(a: &[u8], b: &[u8]) -> usize {
    let min_len = a.len().min(b.len());
    let mut count = a.len().abs_diff(b.len());
    for i in 0..min_len {
        if a[i] != b[i] {
            count += 1;
        }
    }
    count
}

fn validate_partial(data: &[u8]) -> bool {
    zip_util::is_valid_zip_signature(data)
        && matches!(zip_util::parse_and_validate(data), Ok(p) if p.entries.iter().all(|e| e.crc32_actual.map_or(false, |a| a == e.crc32_expected)))
}

fn print_extracted_files(data: &[u8]) -> Result<()> {
    let parsed = zip_util::parse_and_validate(data)?;
    for entry in &parsed.entries {
        let crc_ok = entry
            .crc32_actual
            .map_or(false, |a| a == entry.crc32_expected);
        println!(
            "  {} - {} bytes, CRC32 {} ({:08X})",
            entry.name,
            entry.compressed_size,
            if crc_ok { "OK" } else { "FAIL" },
            entry.crc32_expected,
        );
    }
    Ok(())
}

fn run_corrupt(input: &str, output: &str) -> Result<()> {
    let file = std::fs::File::open(input).context("Failed to open input file")?;
    let mmap = unsafe { memmap2::Mmap::map(&file).context("Failed to memory-map input file")? };
    let data: &[u8] = &mmap;
    println!("Input: {} bytes", data.len());

    let corrupted = corrupt::corrupt(data);
    println!("Corrupted: {} bytes (+{})", corrupted.len(), corrupted.len() - data.len());

    let standalone = corrupt::find_standalone_lf(&data);
    println!("Replaced {} standalone LF bytes", standalone.len());

    std::fs::write(output, &corrupted)?;
    println!("Written to: {}", output);
    Ok(())
}

fn run_recover(input: &str, output: &str) -> Result<()> {
    let file = std::fs::File::open(input).context("Failed to open input file")?;
    let mmap = unsafe { memmap2::Mmap::map(&file).context("Failed to memory-map input file")? };
    let data: &[u8] = &mmap;
    println!("Input: {} bytes", data.len());

    let result = recover::recover(data).context("Recovery failed")?;

    println!("\nStrategy: {}", result.strategy);
    println!("Attempts: {}", result.attempts);
    println!("Recovered: {} bytes", result.data.len());

    std::fs::write(output, &result.data)?;
    println!("Written to: {}", output);

    let parsed = zip_util::parse_and_validate(&result.data)?;
    for entry in &parsed.entries {
        let crc_ok = entry
            .crc32_actual
            .map_or(false, |a| a == entry.crc32_expected);
        println!(
            "  {} - {} bytes compressed, CRC32 {} ({:08X})",
            entry.name,
            entry.compressed_size,
            if crc_ok { "OK" } else { "FAIL" },
            entry.crc32_expected,
        );
    }

    Ok(())
}

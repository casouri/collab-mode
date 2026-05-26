//! Benchmark how long it takes to hash a file under different
//! strategies — informs the choice we make in
//! `handle_external_file_change` for the disk-vs-buffer equality
//! check.
//!
//! Two axes:
//! - source: streamed from disk via BufReader, or already loaded into
//!   the same `GapBuffer<char>` that `Doc` uses internally
//! - algorithm: SHA-256 and MD5
//!
//! Defaults to hashing `collab_mode/src/server.rs`. Override with the
//! first CLI arg.

use std::io::Read;
use std::path::PathBuf;
use std::time::Instant;

use gapbuf::GapBuffer;
use md5::Md5;
use sha2::{Digest, Sha256};

const ITERS: usize = 50;

fn main() -> anyhow::Result<()> {
    let path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("collab_mode/src/server.rs"));
    let bytes = std::fs::metadata(&path)?.len();
    println!("Target: {} ({} bytes)", path.display(), bytes);
    println!("Iterations per measurement: {}", ITERS);
    println!();

    // === Disk: stream + hash ===
    let (disk_sha_us, sha_disk_hex) = bench_disk::<Sha256>(&path)?;
    let (disk_md5_us, md5_disk_hex) = bench_disk::<Md5>(&path)?;

    // === Memory: load to GapBuffer<char>, then hash by walking chars ===
    let content = std::fs::read_to_string(&path)?;
    let mut buffer: GapBuffer<char> = GapBuffer::new();
    buffer.insert_many(0, content.chars());

    let (buf_sha_us, sha_buf_hex) = bench_buffer::<Sha256>(&buffer);
    let (buf_md5_us, md5_buf_hex) = bench_buffer::<Md5>(&buffer);

    // === Diff: collect buffer to String + dissimilar::diff against ===
    // an equal disk-side String. This is the cost
    // `handle_external_file_change` pays today on every
    // already-in-sync FileChanged event (the "empty diff" case).
    let (diff_collect_us, diff_only_us, diff_total_us) = bench_diff(&buffer, &content);

    // === Report ===
    let row = |label: &str, micros: f64, hex: &str| {
        let mb = (bytes as f64) / 1_000_000.0;
        let secs = micros / 1_000_000.0;
        let mbs = if secs > 0.0 { mb / secs } else { 0.0 };
        println!(
            "  {:32}  {:>9.2} µs  ({:>7.1} MB/s)  {}",
            label,
            micros,
            mbs,
            &hex[..16.min(hex.len())],
        );
    };
    println!("Average per iteration:");
    row("disk + SHA-256 (BufReader)", disk_sha_us, &sha_disk_hex);
    row("disk + MD5     (BufReader)", disk_md5_us, &md5_disk_hex);
    row("GapBuffer<char> + SHA-256", buf_sha_us, &sha_buf_hex);
    row("GapBuffer<char> + MD5    ", buf_md5_us, &md5_buf_hex);
    row("buffer→String collect", diff_collect_us, "");
    row("dissimilar::diff (equal)", diff_only_us, "");
    row("buffer→String + diff (equal)", diff_total_us, "");
    println!();

    // Sanity: disk and buffer digests must match for the same algo.
    assert_eq!(
        sha_disk_hex, sha_buf_hex,
        "SHA-256 of disk and buffer differ — UTF-8 encoding mismatch?",
    );
    assert_eq!(md5_disk_hex, md5_buf_hex, "MD5 of disk and buffer differ");
    println!("digests match across sources ✓");
    Ok(())
}

/// Hash the file from disk through a BufReader. Returns
/// (avg microseconds per iteration, final hex digest).
fn bench_disk<D: Digest>(path: &PathBuf) -> anyhow::Result<(f64, String)> {
    let mut total_ns: u128 = 0;
    let mut hex = String::new();
    for _ in 0..ITERS {
        let file = std::fs::File::open(path)?;
        let mut reader = std::io::BufReader::new(file);
        let mut buf = [0u8; 8192];
        let mut hasher = D::new();
        let start = Instant::now();
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let digest = hasher.finalize();
        total_ns += start.elapsed().as_nanos();
        hex = hex_of(&digest);
    }
    Ok((total_ns as f64 / ITERS as f64 / 1000.0, hex))
}

/// Hash the in-memory `GapBuffer<char>` by encoding each char to
/// UTF-8 bytes and feeding the bytes into the hasher.
fn bench_buffer<D: Digest>(buf: &GapBuffer<char>) -> (f64, String) {
    let mut total_ns: u128 = 0;
    let mut hex = String::new();
    for _ in 0..ITERS {
        let mut hasher = D::new();
        let mut utf8 = [0u8; 4];
        let start = Instant::now();
        for ch in buf.iter() {
            let s = ch.encode_utf8(&mut utf8);
            hasher.update(s.as_bytes());
        }
        let digest = hasher.finalize();
        total_ns += start.elapsed().as_nanos();
        hex = hex_of(&digest);
    }
    (total_ns as f64 / ITERS as f64 / 1000.0, hex)
}

/// Measure three things for the equal-content (empty diff) case:
/// (a) time to collect the GapBuffer to a String,
/// (b) time for `dissimilar::diff` on two equal strings, and
/// (c) the combined cost — what `handle_external_file_change` pays
///     today on each "save echo" event before any short-circuit.
fn bench_diff(buf: &GapBuffer<char>, disk_content: &str) -> (f64, f64, f64) {
    let mut collect_ns: u128 = 0;
    let mut diff_ns: u128 = 0;
    let mut total_ns: u128 = 0;

    // We collect the buffer once outside the diff-only loop so that
    // the diff measurement isn't paying for any collect cost.
    let buf_string: String = buf.iter().collect();

    for _ in 0..ITERS {
        // (a) collect
        let s1 = Instant::now();
        let _collected: String = std::hint::black_box(buf.iter().collect());
        collect_ns += s1.elapsed().as_nanos();

        // (b) diff only
        let s2 = Instant::now();
        let chunks = dissimilar::diff(&buf_string, disk_content);
        std::hint::black_box(&chunks);
        diff_ns += s2.elapsed().as_nanos();

        // (c) end-to-end: collect + diff
        let s3 = Instant::now();
        let collected: String = buf.iter().collect();
        let chunks = dissimilar::diff(&collected, disk_content);
        std::hint::black_box(&chunks);
        total_ns += s3.elapsed().as_nanos();
    }
    let to_us = |ns: u128| ns as f64 / ITERS as f64 / 1000.0;
    (to_us(collect_ns), to_us(diff_ns), to_us(total_ns))
}

fn hex_of(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(&mut s, "{:02x}", b).unwrap();
    }
    s
}

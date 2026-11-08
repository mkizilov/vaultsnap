//! Chunking throughput by input size, content type, chunk-size configuration, and API.
//!
//! ```text
//! cargo bench -p vaultsnap-core --bench chunking              # 4 KiB .. 64 MiB
//! cargo bench -p vaultsnap-core --bench chunking -- --quick   # 4 KiB .. 1 MiB
//! cargo bench -p vaultsnap-core --bench chunking -- --large   # adds 256 MiB
//! ```
//!
//! Everything is generated in memory and dropped again, so nothing touches the disk and memory
//! use is bounded by the largest input (64 MiB by default, 256 MiB with `--large`) plus the
//! chunker's buffer (at most 2 x the maximum chunk size, 8 MiB for the default configuration).
//! Each row is the median of five timed runs, each run chunking at least 64 MiB in total (or
//! one pass for larger inputs). The numbers depend on the machine; compare rows, not machines.

use std::hint::black_box;
use std::time::Instant;

use vaultsnap_core::chunking::{Chunker, ChunkerConfig};

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;
const SAMPLES: usize = 5;
const BYTES_PER_SAMPLE: usize = 64 * MIB;

#[derive(Clone, Copy)]
enum Content {
    Random,
    Zeros,
    Text,
    Counter,
    LowEntropy,
}

impl Content {
    const ALL: [Self; 5] = [
        Self::Random,
        Self::Zeros,
        Self::Text,
        Self::Counter,
        Self::LowEntropy,
    ];

    fn name(self) -> &'static str {
        match self {
            Self::Random => "random",
            Self::Zeros => "zeros",
            Self::Text => "text",
            Self::Counter => "counter",
            Self::LowEntropy => "low-entropy",
        }
    }

    fn generate(self, len: usize) -> Vec<u8> {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        match self {
            // Incompressible data such as encrypted or already compressed files.
            Self::Random => rng.bytes(len),
            // Sparse files and preallocated space.
            Self::Zeros => vec![0; len],
            // Prose-like text built from a small vocabulary.
            Self::Text => {
                const WORDS: [&str; 16] = [
                    "backup", "chunk", "snapshot", "the", "of", "repository", "encrypted", "and",
                    "incremental", "restore", "data", "a", "file", "boundary", "hash", "stream",
                ];
                let mut out = Vec::with_capacity(len + 16);
                while out.len() < len {
                    out.extend_from_slice(WORDS[rng.below(WORDS.len())].as_bytes());
                    out.push(if rng.below(12) == 0 { b'\n' } else { b' ' });
                }
                out.truncate(len);
                out
            }
            // Database pages or arrays of integers: highly structured binary.
            Self::Counter => (0_u32..)
                .flat_map(u32::to_le_bytes)
                .take(len)
                .collect(),
            // Bitmaps, logs with few distinct bytes.
            Self::LowEntropy => rng.bytes(len).into_iter().map(|byte| byte & 3).collect(),
        }
    }
}

/// xorshift64.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: usize) -> usize {
        (self.next_u64() % bound as u64) as usize
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(len + 8);
        while out.len() < len {
            out.extend_from_slice(&self.next_u64().to_le_bytes());
        }
        out.truncate(len);
        out
    }
}

#[derive(Clone, Copy)]
enum Api {
    /// `Chunker` over a `Read`, as used for files.
    Stream,
    /// `ChunkerConfig::split` over an in-memory buffer.
    Slice,
}

impl Api {
    fn name(self) -> &'static str {
        match self {
            Self::Stream => "stream",
            Self::Slice => "slice",
        }
    }

    /// Chunks `data` once and returns how many chunks there were.
    fn run(self, config: ChunkerConfig, data: &[u8]) -> usize {
        match self {
            Self::Stream => {
                let mut chunker = Chunker::new(data, config);
                let mut count = 0;
                while let Some(chunk) = chunker.next_chunk().expect("reading a slice cannot fail") {
                    black_box(chunk.data);
                    count += 1;
                }
                count
            }
            Self::Slice => {
                let ranges = config.split(data);
                black_box(&ranges);
                ranges.len()
            }
        }
    }
}

fn human(bytes: usize) -> String {
    if bytes >= MIB {
        format!("{} MiB", bytes / MIB)
    } else {
        format!("{} KiB", bytes / KIB)
    }
}

fn median(samples: &mut [f64]) -> f64 {
    samples.sort_by(f64::total_cmp);
    samples[samples.len() / 2]
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let quick = args.iter().any(|arg| arg == "--quick");
    let large = args.iter().any(|arg| arg == "--large");

    let mut sizes = vec![4 * KIB, 64 * KIB, MIB];
    if !quick {
        sizes.extend([16 * MIB, 64 * MIB]);
    }
    if large && !quick {
        sizes.push(256 * MIB);
    }
    let configs = [
        ("default 256K/1M/4M", ChunkerConfig::default()),
        (
            "small 2K/8K/32K",
            ChunkerConfig::new(2 * KIB, 8 * KIB, 32 * KIB).expect("valid config"),
        ),
    ];

    println!(
        "{:<12} {:>9} {:<19} {:<7} {:>9} {:>11} {:>10}",
        "content", "size", "config", "api", "chunks", "avg chunk", "MiB/s"
    );
    for size in sizes {
        for content in Content::ALL {
            let data = content.generate(size);
            for (config_name, config) in configs {
                for api in [Api::Stream, Api::Slice] {
                    let runs = (BYTES_PER_SAMPLE / size).max(1);
                    let mut chunks = 0;
                    let mut throughput = Vec::with_capacity(SAMPLES);
                    for _ in 0..SAMPLES {
                        let started = Instant::now();
                        for _ in 0..runs {
                            chunks = api.run(config, black_box(&data));
                        }
                        let seconds = started.elapsed().as_secs_f64();
                        throughput.push((size * runs) as f64 / MIB as f64 / seconds);
                    }
                    println!(
                        "{:<12} {:>9} {:<19} {:<7} {:>9} {:>11} {:>10.0}",
                        content.name(),
                        human(size),
                        config_name,
                        api.name(),
                        chunks,
                        human(size / chunks.max(1)),
                        median(&mut throughput),
                    );
                }
            }
        }
    }
}

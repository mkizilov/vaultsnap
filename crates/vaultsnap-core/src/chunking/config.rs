//! Chunk size limits and the cut-point search (FastCDC with normalized chunking).
//!
//! The rule, which is part of the repository format (see `docs/FORMAT.md`):
//!
//! * A chunk is at least `min_size` and at most `max_size` bytes, except that the last chunk of a
//!   stream may be shorter than `min_size`.
//! * A rolling hash `h = (h << 1) + GEAR[byte]` (wrapping, 64 bits) is kept over the bytes of the
//!   chunk. It starts 64 bytes before the minimum size, so that by then every older byte has been
//!   shifted out and `h` depends only on the last 64 bytes.
//! * For a candidate length `n` with `min_size <= n < max_size`, the chunk ends after `n` bytes
//!   if `h & mask == 0`, where `mask` is the *strict* mask while `n < avg_size` and the *loose*
//!   mask afterwards. The strict mask has two more bits than `log2(avg_size)`, the loose mask two
//!   fewer, which pulls chunk sizes towards the average (normalized chunking).
//! * If no candidate matches, the chunk ends at exactly `max_size` bytes.
//!
//! The decision at every position therefore depends only on the preceding 64 bytes and on the
//! length of the chunk so far; an edit can only disturb the chunks it touches, and boundaries
//! re-align shortly after it.

use std::ops::Range;

use super::gear::GEAR;
use crate::error::{Error, Result};

/// Bytes of history the rolling hash depends on (the width of the hash register).
const WINDOW: usize = 64;
/// How many bits stricter (before the average size) and looser (after it) the masks are.
const NORMALIZATION: u32 = 2;

/// Minimum, average and maximum chunk size.
///
/// The average is a target, not a guarantee: actual chunks average somewhat above it on random
/// data. Use [`ChunkerConfig::default`] unless there is a reason not to; chunks produced with
/// different settings do not deduplicate against each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkerConfig {
    min_size: usize,
    avg_size: usize,
    max_size: usize,
    strict_mask: u64,
    loose_mask: u64,
}

impl ChunkerConfig {
    /// Smallest allowed minimum chunk size (the hash window).
    pub const MIN_SIZE_LIMIT: usize = WINDOW;
    /// Smallest allowed average chunk size.
    pub const AVG_SIZE_LIMIT: usize = 256;
    /// Largest allowed maximum chunk size. This also bounds the memory a chunker uses.
    pub const MAX_SIZE_LIMIT: usize = 64 * 1024 * 1024;

    /// Validates and builds a configuration: `64 <= min <= avg <= max <= 64 MiB` and `avg >= 256`.
    pub fn new(min_size: usize, avg_size: usize, max_size: usize) -> Result<Self> {
        let reject = |message: String| Err(Error::InvalidChunkerConfig(message));
        if min_size < Self::MIN_SIZE_LIMIT {
            return reject(format!(
                "minimum size {min_size} is below the limit of {}",
                Self::MIN_SIZE_LIMIT
            ));
        }
        if avg_size < Self::AVG_SIZE_LIMIT {
            return reject(format!(
                "average size {avg_size} is below the limit of {}",
                Self::AVG_SIZE_LIMIT
            ));
        }
        if max_size > Self::MAX_SIZE_LIMIT {
            return reject(format!(
                "maximum size {max_size} is above the limit of {}",
                Self::MAX_SIZE_LIMIT
            ));
        }
        if avg_size < min_size {
            return reject(format!(
                "average size {avg_size} is below the minimum size {min_size}"
            ));
        }
        if max_size < avg_size {
            return reject(format!(
                "maximum size {max_size} is below the average size {avg_size}"
            ));
        }
        let bits = nearest_log2(avg_size);
        Ok(Self {
            min_size,
            avg_size,
            max_size,
            strict_mask: top_bits(bits + NORMALIZATION),
            loose_mask: top_bits(bits - NORMALIZATION),
        })
    }

    /// No chunk is shorter than this, except possibly the last one.
    pub fn min_size(&self) -> usize {
        self.min_size
    }

    /// The target average chunk size.
    pub fn avg_size(&self) -> usize {
        self.avg_size
    }

    /// No chunk is longer than this.
    pub fn max_size(&self) -> usize {
        self.max_size
    }

    /// Length of the first chunk of `data`, which must begin at a chunk boundary.
    ///
    /// Returns `None` when `data` ends before a cut point is found, i.e. when more input is
    /// needed; if the input is exhausted the caller takes all of `data` as the last chunk. Only
    /// the first `max_size` bytes are examined, so `Some` is always returned for
    /// `data.len() >= max_size`. The result depends only on the bytes up to the cut.
    pub fn cut_point(&self, data: &[u8]) -> Option<usize> {
        let limit = data.len().min(self.max_size);
        if limit < self.min_size {
            return None;
        }
        let mut hash = 0_u64;
        for &byte in &data[self.min_size - WINDOW..self.min_size - 1] {
            hash = (hash << 1).wrapping_add(GEAR[usize::from(byte)]);
        }
        for (index, &byte) in data[self.min_size - 1..limit].iter().enumerate() {
            let length = self.min_size + index;
            hash = (hash << 1).wrapping_add(GEAR[usize::from(byte)]);
            if length >= self.max_size {
                return Some(self.max_size);
            }
            let mask = if length < self.avg_size {
                self.strict_mask
            } else {
                self.loose_mask
            };
            if hash & mask == 0 {
                return Some(length);
            }
        }
        None
    }

    /// Splits an in-memory buffer into chunks, returned as byte ranges. Equivalent to running a
    /// [`Chunker`](super::Chunker) over the same bytes. Empty input yields no chunks.
    pub fn split(&self, data: &[u8]) -> Vec<Range<usize>> {
        let mut ranges = Vec::new();
        let mut start = 0;
        while start < data.len() {
            let rest = &data[start..];
            let length = self.cut_point(rest).unwrap_or(rest.len());
            ranges.push(start..start + length);
            start += length;
        }
        ranges
    }
}

impl Default for ChunkerConfig {
    /// 256 KiB minimum, 1 MiB average, 4 MiB maximum.
    fn default() -> Self {
        Self::new(256 * 1024, 1024 * 1024, 4 * 1024 * 1024).expect("default sizes are valid")
    }
}

/// `log2(value)` rounded to the nearest integer, without floating point.
fn nearest_log2(value: usize) -> u32 {
    let floor = usize::BITS - 1 - value.leading_zeros();
    // Round up when value >= 2^floor * sqrt(2), i.e. value^2 >= 2^(2 * floor + 1).
    let square = (value as u128) * (value as u128);
    if square >= 1_u128 << (2 * floor + 1) {
        floor + 1
    } else {
        floor
    }
}

/// A mask with the `bits` most significant bits set.
fn top_bits(bits: u32) -> u64 {
    u64::MAX << (u64::BITS - bits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunking::test_util::pseudo_random;

    fn config(min: usize, avg: usize, max: usize) -> ChunkerConfig {
        ChunkerConfig::new(min, avg, max).expect("valid config")
    }

    #[test]
    fn default_sizes_match_the_format_document() {
        let config = ChunkerConfig::default();
        assert_eq!(config.min_size(), 256 * 1024);
        assert_eq!(config.avg_size(), 1024 * 1024);
        assert_eq!(config.max_size(), 4 * 1024 * 1024);
    }

    #[test]
    fn valid_configurations_are_accepted_including_degenerate_ones() {
        assert!(ChunkerConfig::new(64, 256, 256).is_ok());
        assert!(ChunkerConfig::new(256, 256, 256).is_ok());
        assert!(ChunkerConfig::new(64, 256, ChunkerConfig::MAX_SIZE_LIMIT).is_ok());
        assert!(ChunkerConfig::new(1000, 3000, 7000).is_ok());
    }

    #[test]
    fn invalid_configurations_are_rejected() {
        let cases = [
            (63, 256, 1024),
            (0, 256, 1024),
            (64, 255, 1024),
            (512, 256, 1024),
            (64, 2048, 1024),
            (64, 256, ChunkerConfig::MAX_SIZE_LIMIT + 1),
            (usize::MAX, usize::MAX, usize::MAX),
        ];
        for (min, avg, max) in cases {
            let result = ChunkerConfig::new(min, avg, max);
            assert!(
                matches!(result, Err(Error::InvalidChunkerConfig(_))),
                "{min} {avg} {max}"
            );
        }
    }

    #[test]
    fn the_average_size_is_rounded_to_the_nearest_power_of_two_in_log_space() {
        assert_eq!(nearest_log2(256), 8);
        assert_eq!(nearest_log2(1024 * 1024), 20);
        assert_eq!(nearest_log2(2896), 11);
        assert_eq!(nearest_log2(2897), 12);
        assert_eq!(nearest_log2(4095), 12);
        assert_eq!(nearest_log2(ChunkerConfig::MAX_SIZE_LIMIT), 26);
    }

    #[test]
    fn masks_are_two_bits_stricter_and_looser_than_the_average() {
        let config = config(64, 256, 1024);
        assert_eq!(config.strict_mask.count_ones(), 10);
        assert_eq!(config.loose_mask.count_ones(), 6);
        assert_eq!(config.strict_mask & config.loose_mask, config.loose_mask);
        assert_eq!(config.strict_mask >> 54, 0x3ff);
        assert_eq!(config.strict_mask << 10, 0);
    }

    #[test]
    fn short_input_never_has_a_cut_point() {
        let config = config(64, 256, 1024);
        let data = pseudo_random(2000, 7);
        for len in [0, 1, 63] {
            assert_eq!(config.cut_point(&data[..len]), None, "len {len}");
        }
    }

    #[test]
    fn cut_points_respect_the_limits() {
        let config = config(64, 256, 1024);
        for seed in 0..50 {
            let data = pseudo_random(4096, seed);
            let cut = config
                .cut_point(&data)
                .expect("input is longer than the maximum");
            assert!((64..=1024).contains(&cut), "cut {cut}");
        }
    }

    #[test]
    fn the_cut_depends_only_on_the_bytes_up_to_it() {
        let config = config(64, 256, 1024);
        for seed in 0..50 {
            let data = pseudo_random(4096, seed);
            let cut = config.cut_point(&data).expect("cut");
            assert_eq!(config.cut_point(&data[..cut]), Some(cut), "seed {seed}");
            if cut < config.max_size() {
                assert_eq!(config.cut_point(&data[..cut - 1]), None, "seed {seed}");
            }
        }
    }

    #[test]
    fn bytes_older_than_the_hash_window_do_not_influence_the_cut() {
        let config = config(256, 1024, 4096);
        for seed in 0..50 {
            let original = pseudo_random(8192, seed);
            let mut altered = original.clone();
            let noise = pseudo_random(config.min_size() - WINDOW, seed + 1000);
            altered[..noise.len()].copy_from_slice(&noise);
            assert_eq!(
                config.cut_point(&original),
                config.cut_point(&altered),
                "seed {seed}"
            );
        }
    }

    #[test]
    fn equal_minimum_and_maximum_gives_fixed_size_chunks() {
        let config = config(256, 256, 256);
        let data = pseudo_random(1000, 3);
        assert_eq!(config.cut_point(&data), Some(256));
        let lengths: Vec<usize> = config
            .split(&data)
            .iter()
            .map(ExactSizeIterator::len)
            .collect();
        assert_eq!(lengths, [256, 256, 256, 232]);
    }

    #[test]
    fn split_covers_the_input_exactly_and_respects_the_limits() {
        let config = config(64, 256, 1024);
        for len in [0, 1, 63, 64, 65, 1023, 1024, 1025, 10_000] {
            let data = pseudo_random(len, len as u64);
            let ranges = config.split(&data);
            let mut expected_start = 0;
            for (index, range) in ranges.iter().enumerate() {
                assert_eq!(range.start, expected_start);
                assert!(range.len() <= 1024 && !range.is_empty());
                if index + 1 < ranges.len() {
                    assert!(range.len() >= 64, "interior chunk below the minimum");
                }
                expected_start = range.end;
            }
            assert_eq!(expected_start, len);
        }
    }

    #[test]
    fn constant_input_yields_equal_chunks() {
        let config = config(64, 256, 1024);
        for byte in [0_u8, 1, 0x55, 0xff] {
            let data = vec![byte; 20_000];
            let ranges = config.split(&data);
            let first = ranges[0].len();
            let last = ranges.len() - 1;
            assert!(
                ranges[..last].iter().all(|range| range.len() == first),
                "byte {byte}"
            );
            assert!(ranges[last].len() <= first);
        }
    }
}

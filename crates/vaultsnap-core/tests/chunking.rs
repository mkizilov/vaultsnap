//! Property-based and statistical tests of the chunking engine through its public API.

use std::collections::HashSet;
use std::io::{self, Cursor, Read};
use std::ops::Range;

use proptest::prelude::*;
use vaultsnap_core::chunking::{Chunker, ChunkerConfig};

/// xorshift64: reproducible test data and decisions.
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

/// Serves the data in pieces whose sizes cycle through `sizes` (all at least 1).
struct Fragmented<'a> {
    data: &'a [u8],
    position: usize,
    sizes: Vec<usize>,
    turn: usize,
}

impl Read for Fragmented<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let size = self.sizes[self.turn % self.sizes.len()];
        self.turn += 1;
        let count = size.min(buf.len()).min(self.data.len() - self.position);
        buf[..count].copy_from_slice(&self.data[self.position..self.position + count]);
        self.position += count;
        Ok(count)
    }
}

/// Chunks `reader` and returns the (offset-checked) ranges and the concatenated bytes.
fn run_stream<R: Read>(reader: R, config: ChunkerConfig) -> (Vec<Range<usize>>, Vec<u8>) {
    let mut ranges = Vec::new();
    let mut joined = Vec::new();
    for chunk in Chunker::new(reader, config) {
        let chunk = chunk.expect("read chunk");
        assert_eq!(chunk.offset, joined.len() as u64, "chunk offsets are contiguous");
        ranges.push(joined.len()..joined.len() + chunk.data.len());
        joined.extend_from_slice(&chunk.data);
    }
    (ranges, joined)
}

fn config_strategy() -> impl Strategy<Value = ChunkerConfig> {
    (64_usize..=256, 256_usize..=1024, 1024_usize..=4096)
        .prop_map(|(min, avg, max)| ChunkerConfig::new(min, avg, max).expect("valid config"))
}

/// Random bytes, a tiny alphabet, and a single repeated byte.
fn data_strategy() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        prop::collection::vec(any::<u8>(), 0..20_000),
        prop::collection::vec(0_u8..3, 0..20_000),
        (any::<u8>(), 0_usize..20_000).prop_map(|(byte, len)| vec![byte; len]),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn reconstructs_every_byte_and_respects_the_size_limits(
        config in config_strategy(),
        data in data_strategy(),
    ) {
        let (ranges, joined) = run_stream(Cursor::new(&data), config);
        prop_assert_eq!(&joined, &data);
        prop_assert_eq!(ranges.is_empty(), data.is_empty());
        for (index, range) in ranges.iter().enumerate() {
            prop_assert!(!range.is_empty());
            prop_assert!(range.len() <= config.max_size());
            if index + 1 < ranges.len() {
                prop_assert!(range.len() >= config.min_size());
            }
        }
    }

    #[test]
    fn boundaries_do_not_depend_on_how_the_reader_splits_its_reads(
        config in config_strategy(),
        data in data_strategy(),
        sizes in prop::collection::vec(1_usize..700, 1..8),
    ) {
        let (whole, _) = run_stream(Cursor::new(&data), config);
        let fragmented = Fragmented { data: &data, position: 0, sizes, turn: 0 };
        let (pieces, joined) = run_stream(fragmented, config);
        prop_assert_eq!(&pieces, &whole);
        prop_assert_eq!(&joined, &data);
    }

    #[test]
    fn streaming_and_in_memory_splitting_agree_and_are_deterministic(
        config in config_strategy(),
        data in data_strategy(),
    ) {
        let (streamed, _) = run_stream(Cursor::new(&data), config);
        prop_assert_eq!(&streamed, &config.split(&data));
        prop_assert_eq!(&config.split(&data), &config.split(&data));
    }

    #[test]
    fn an_insertion_leaves_every_chunk_before_it_unchanged(
        config in config_strategy(),
        data in prop::collection::vec(any::<u8>(), 0..20_000),
        position in any::<usize>(),
        inserted in prop::collection::vec(any::<u8>(), 1..64),
    ) {
        let position = position % (data.len() + 1);
        let mut modified = data[..position].to_vec();
        modified.extend_from_slice(&inserted);
        modified.extend_from_slice(&data[position..]);

        // The final chunk is excluded: it may end at the end of the data rather than at a
        // content-defined boundary, and then more data can legitimately extend it.
        let before: Vec<Range<usize>> = config
            .split(&data)
            .into_iter()
            .take_while(|range| range.end <= position && range.end < data.len())
            .collect();
        let after = config.split(&modified);
        prop_assert!(after.len() >= before.len());
        prop_assert_eq!(&after[..before.len()], &before[..]);
    }

    #[test]
    fn a_deletion_leaves_every_chunk_before_it_unchanged(
        config in config_strategy(),
        data in prop::collection::vec(any::<u8>(), 1..20_000),
        position in any::<usize>(),
        removed in 1_usize..64,
    ) {
        let position = position % data.len();
        let end = (position + removed).min(data.len());
        let mut modified = data[..position].to_vec();
        modified.extend_from_slice(&data[end..]);

        let before: Vec<Range<usize>> = config
            .split(&data)
            .into_iter()
            .take_while(|range| range.end <= position)
            .collect();
        let after = config.split(&modified);
        prop_assert!(after.len() >= before.len());
        prop_assert_eq!(&after[..before.len()], &before[..]);
    }

    #[test]
    fn appending_data_only_changes_the_last_chunk(
        config in config_strategy(),
        data in data_strategy(),
        extra in prop::collection::vec(any::<u8>(), 1..5000),
    ) {
        let original = config.split(&data);
        let mut extended = data.clone();
        extended.extend_from_slice(&extra);
        let grown = config.split(&extended);
        let stable = original.len().saturating_sub(1);
        prop_assert!(grown.len() >= stable);
        prop_assert_eq!(&grown[..stable], &original[..stable]);
    }

    #[test]
    fn constant_data_is_cut_into_equal_chunks(
        config in config_strategy(),
        byte in any::<u8>(),
        len in 0_usize..20_000,
    ) {
        let ranges = config.split(&vec![byte; len]);
        if let Some((last, rest)) = ranges.split_last() {
            let first = rest.first().map_or(last.len(), |range| range.len());
            prop_assert!(rest.iter().all(|range| range.len() == first));
            prop_assert!(last.len() <= first);
        }
    }
}

/// Fraction of `modified`, by bytes, that sits in chunks which also occur in `original`.
fn preserved_fraction(config: &ChunkerConfig, original: &[u8], modified: &[u8]) -> f64 {
    let known: HashSet<&[u8]> = config
        .split(original)
        .into_iter()
        .map(|range| &original[range])
        .collect();
    let kept: usize = config
        .split(modified)
        .into_iter()
        .map(|range| &modified[range])
        .filter(|chunk| known.contains(chunk))
        .map(<[u8]>::len)
        .sum();
    kept as f64 / modified.len() as f64
}

fn test_config() -> ChunkerConfig {
    ChunkerConfig::new(1024, 4096, 16_384).expect("valid config")
}

#[test]
fn small_insertions_preserve_almost_every_chunk() {
    let config = test_config();
    let mut rng = Rng(0x0123_4567_89ab_cdef);
    let original = rng.bytes(1 << 20);
    let trials: u32 = 40;
    let mut total = 0.0;
    for trial in 0..trials {
        let position = rng.below(original.len() + 1);
        let insertion_len = 1 + rng.below(64);
        let insertion = rng.bytes(insertion_len);
        let mut modified = original[..position].to_vec();
        modified.extend_from_slice(&insertion);
        modified.extend_from_slice(&original[position..]);

        let kept = preserved_fraction(&config, &original, &modified);
        assert!(kept >= 0.90, "trial {trial}: only {kept:.3} preserved (insert at {position})");
        total += kept;
    }
    let mean = total / f64::from(trials);
    assert!(mean >= 0.97, "on average only {mean:.4} of the data was preserved");
}

#[test]
fn small_deletions_preserve_almost_every_chunk() {
    let config = test_config();
    let mut rng = Rng(0xfeed_face_cafe_beef);
    let original = rng.bytes(1 << 20);
    let trials: u32 = 40;
    let mut total = 0.0;
    for trial in 0..trials {
        let position = rng.below(original.len());
        let end = (position + 1 + rng.below(64)).min(original.len());
        let mut modified = original[..position].to_vec();
        modified.extend_from_slice(&original[end..]);

        let kept = preserved_fraction(&config, &original, &modified);
        assert!(kept >= 0.90, "trial {trial}: only {kept:.3} preserved (delete at {position})");
        total += kept;
    }
    let mean = total / f64::from(trials);
    assert!(mean >= 0.97, "on average only {mean:.4} of the data was preserved");
}

#[test]
fn prepending_bytes_shifts_every_offset_but_keeps_the_chunks() {
    let config = test_config();
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    let original = rng.bytes(1 << 20);
    for prefix_len in [1, 7, 100, 4096, 5000] {
        let mut modified = rng.bytes(prefix_len);
        modified.extend_from_slice(&original);
        let kept = preserved_fraction(&config, &original, &modified);
        assert!(kept >= 0.97, "prefix of {prefix_len} bytes: only {kept:.3} preserved");
    }
}

#[test]
fn mixed_content_types_are_chunked_consistently() {
    let config = ChunkerConfig::new(256, 1024, 4096).expect("valid config");
    let mut rng = Rng(7);
    const SENTENCE: &[u8] = b"the quick brown fox jumps over the lazy dog\n";
    let text: Vec<u8> = (0..200_000)
        .map(|i| SENTENCE[i % SENTENCE.len()] ^ u8::from(rng.below(8) == 0))
        .collect();
    let counter: Vec<u8> = (0_u32..50_000).flat_map(u32::to_le_bytes).collect();
    let sparse: Vec<u8> = (0..200_000).map(|_| u8::from(rng.below(50) == 0)).collect();
    let mut mixed = rng.bytes(30_000);
    mixed.extend_from_slice(&text);
    mixed.extend_from_slice(&counter);
    mixed.extend_from_slice(&[0_u8; 40_000]);
    mixed.extend_from_slice(&sparse);

    for (name, data) in [("text", text), ("counter", counter), ("sparse", sparse), ("mixed", mixed)] {
        let (ranges, joined) = run_stream(Cursor::new(&data), config);
        assert_eq!(joined, data, "{name}");
        assert_eq!(ranges, config.split(&data), "{name}");
        assert!(ranges.iter().all(|range| range.len() <= config.max_size()), "{name}");
    }
}

#[test]
fn very_large_streams_are_chunked_exactly_without_being_held_in_memory() {
    // 64 MiB generated on the fly, so the test itself never holds it. The chunker's own buffer
    // bound (2 * max_size) is asserted by a unit test that can see the buffer.
    struct Generated {
        remaining: u64,
        rng: Rng,
    }
    impl Read for Generated {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let count = buf.len().min(usize::try_from(self.remaining).unwrap_or(usize::MAX));
            for chunk in buf[..count].chunks_mut(8) {
                let word = self.rng.next_u64().to_le_bytes();
                chunk.copy_from_slice(&word[..chunk.len()]);
            }
            self.remaining -= count as u64;
            Ok(count)
        }
    }

    let config = ChunkerConfig::default();
    let total: u64 = 64 * 1024 * 1024;
    let source = Generated { remaining: total, rng: Rng(99) };
    let mut seen = 0_u64;
    let mut count = 0_u64;
    let mut chunker = Chunker::new(source, config);
    while let Some(chunk) = chunker.next_chunk().expect("read") {
        assert_eq!(chunk.offset, seen);
        assert!(chunk.data.len() >= config.min_size() || seen + chunk.data.len() as u64 == total);
        assert!(chunk.data.len() <= config.max_size());
        seen += chunk.data.len() as u64;
        count += 1;
    }
    assert_eq!(seen, total);
    assert!((16..=257).contains(&count), "{count} chunks for 64 MiB");
}

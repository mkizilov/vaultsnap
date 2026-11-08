//! Streaming chunker over any [`Read`]. It knows nothing about files, repositories, or storage.

use std::io::{self, ErrorKind, Read};

use super::config::ChunkerConfig;

/// First buffer allocation. Small inputs never pay for a full-size buffer.
const INITIAL_BUFFER: usize = 64 * 1024;
/// Do not issue reads into less free space than this (capped by the maximum chunk size).
const MIN_READ: usize = 4096;

/// A chunk borrowed from the chunker's buffer; valid until the next call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk<'a> {
    /// Position of the first byte of the chunk in the stream.
    pub offset: u64,
    /// The chunk's bytes.
    pub data: &'a [u8],
}

/// An owned chunk, as yielded by the [`Iterator`] implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkBuf {
    /// Position of the first byte of the chunk in the stream.
    pub offset: u64,
    /// The chunk's bytes.
    pub data: Vec<u8>,
}

/// Splits a byte stream into content-defined chunks.
///
/// * **Streaming and bounded memory**: the reader is consumed incrementally and at most
///   `2 * max_size` bytes are buffered, however large the stream is. The buffer grows only as
///   far as the input needs, so small inputs stay cheap.
/// * **Deterministic**: boundaries depend only on the bytes and the [`ChunkerConfig`], never on
///   how the reader splits its reads.
/// * **Binary safe**: bytes are never interpreted.
/// * An empty stream yields no chunks; a stream shorter than the minimum size yields one chunk.
///
/// After a read error the error is returned once and the chunker yields nothing further; chunks
/// still buffered at that point are discarded, so callers must treat the stream as failed.
#[derive(Debug)]
pub struct Chunker<R> {
    reader: R,
    config: ChunkerConfig,
    buf: Vec<u8>,
    /// `buf[start..end]` holds bytes read but not yet emitted.
    start: usize,
    end: usize,
    /// Stream position of `buf[start]`.
    offset: u64,
    eof: bool,
    failed: bool,
}

impl<R: Read> Chunker<R> {
    /// Chunks `reader` according to `config`. Nothing is read until the first chunk is requested.
    pub fn new(reader: R, config: ChunkerConfig) -> Self {
        Self {
            reader,
            config,
            buf: Vec::new(),
            start: 0,
            end: 0,
            offset: 0,
            eof: false,
            failed: false,
        }
    }

    /// The configuration in use.
    pub fn config(&self) -> &ChunkerConfig {
        &self.config
    }

    /// The next chunk, borrowed from the internal buffer (no allocation per chunk), or `None`
    /// at the end of the stream.
    pub fn next_chunk(&mut self) -> io::Result<Option<Chunk<'_>>> {
        if self.failed {
            return Ok(None);
        }
        if let Err(err) = self.fill() {
            self.failed = true;
            return Err(err);
        }
        let available = self.end - self.start;
        if available == 0 {
            return Ok(None);
        }
        let window = &self.buf[self.start..self.end];
        let length = self.config.cut_point(window).unwrap_or(available);
        let begin = self.start;
        let offset = self.offset;
        self.start += length;
        self.offset += length as u64;
        Ok(Some(Chunk {
            offset,
            data: &self.buf[begin..begin + length],
        }))
    }

    /// Reads until at least `max_size` bytes are buffered or the stream ends, so that the cut
    /// search always sees enough data to decide.
    fn fill(&mut self) -> io::Result<()> {
        let max = self.config.max_size();
        let limit = max * 2;
        let min_read = MIN_READ.min(max);
        while !self.eof && self.end - self.start < max {
            if self.buf.len() - self.end < min_read {
                if self.start > 0 {
                    self.buf.copy_within(self.start..self.end, 0);
                    self.end -= self.start;
                    self.start = 0;
                } else if self.buf.len() < limit {
                    let grown = (self.buf.len() * 2).max(INITIAL_BUFFER).min(limit);
                    self.buf.resize(grown, 0);
                }
            }
            if self.end == self.buf.len() {
                // Unreachable by construction; stopping here is better than mistaking an empty
                // read buffer (which returns 0) for the end of the stream.
                return Err(io::Error::other("chunker buffer unexpectedly full"));
            }
            match self.reader.read(&mut self.buf[self.end..]) {
                Ok(0) => self.eof = true,
                Ok(read) => self.end += read,
                Err(err) if err.kind() == ErrorKind::Interrupted => {}
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }
}

impl<R: Read> Iterator for Chunker<R> {
    type Item = io::Result<ChunkBuf>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.next_chunk() {
            Ok(Some(chunk)) => Some(Ok(ChunkBuf {
                offset: chunk.offset,
                data: chunk.data.to_vec(),
            })),
            Ok(None) => None,
            Err(err) => Some(Err(err)),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::chunking::test_util::pseudo_random;

    fn config(min: usize, avg: usize, max: usize) -> ChunkerConfig {
        ChunkerConfig::new(min, avg, max).expect("valid config")
    }

    fn small() -> ChunkerConfig {
        config(64, 256, 1024)
    }

    fn collect<R: Read>(reader: R, config: ChunkerConfig) -> Vec<ChunkBuf> {
        Chunker::new(reader, config)
            .collect::<io::Result<Vec<_>>>()
            .expect("read")
    }

    fn lengths(chunks: &[ChunkBuf]) -> Vec<usize> {
        chunks.iter().map(|chunk| chunk.data.len()).collect()
    }

    fn join(chunks: &[ChunkBuf]) -> Vec<u8> {
        chunks
            .iter()
            .flat_map(|chunk| chunk.data.iter().copied())
            .collect()
    }

    fn check_invariants(chunks: &[ChunkBuf], config: &ChunkerConfig) {
        let mut expected_offset = 0_u64;
        for (index, chunk) in chunks.iter().enumerate() {
            assert_eq!(chunk.offset, expected_offset);
            assert!(!chunk.data.is_empty());
            assert!(chunk.data.len() <= config.max_size());
            if index + 1 < chunks.len() {
                assert!(chunk.data.len() >= config.min_size());
            }
            expected_offset += chunk.data.len() as u64;
        }
    }

    /// Returns at most `step` bytes per read.
    struct Trickle<'a> {
        data: &'a [u8],
        position: usize,
        step: usize,
    }

    impl Read for Trickle<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let count = self
                .step
                .min(buf.len())
                .min(self.data.len() - self.position);
            buf[..count].copy_from_slice(&self.data[self.position..self.position + count]);
            self.position += count;
            Ok(count)
        }
    }

    /// Fails with `Interrupted` before every successful read.
    struct Interrupting<'a> {
        inner: Trickle<'a>,
        interrupt_next: bool,
    }

    impl Read for Interrupting<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.interrupt_next = !self.interrupt_next;
            if self.interrupt_next {
                return Err(io::Error::from(ErrorKind::Interrupted));
            }
            self.inner.read(buf)
        }
    }

    /// Serves `good` bytes of data, then fails.
    struct Failing<'a> {
        inner: Trickle<'a>,
        good: usize,
    }

    impl Read for Failing<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.inner.position >= self.good {
                return Err(io::Error::other("disk on fire"));
            }
            self.inner.read(buf)
        }
    }

    /// Generates `remaining` pseudo-random bytes without ever holding them all.
    struct Endless {
        remaining: u64,
        state: u64,
    }

    impl Read for Endless {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let count = buf
                .len()
                .min(usize::try_from(self.remaining).unwrap_or(usize::MAX));
            for byte in &mut buf[..count] {
                self.state ^= self.state << 13;
                self.state ^= self.state >> 7;
                self.state ^= self.state << 17;
                *byte = (self.state >> 24) as u8;
            }
            self.remaining -= count as u64;
            Ok(count)
        }
    }

    #[test]
    fn an_empty_stream_yields_no_chunks() {
        assert!(collect(Cursor::new(Vec::new()), small()).is_empty());
        assert!(collect(Cursor::new(Vec::new()), ChunkerConfig::default()).is_empty());
    }

    #[test]
    fn streams_shorter_than_the_minimum_are_one_chunk() {
        for len in [1, 2, 63] {
            let data = pseudo_random(len, 9);
            let chunks = collect(Cursor::new(data.clone()), small());
            assert_eq!(chunks, [ChunkBuf { offset: 0, data }], "len {len}");
        }
    }

    #[test]
    fn small_streams_stay_small_in_memory() {
        let data = pseudo_random(100, 1);
        let mut chunker = Chunker::new(Cursor::new(data), ChunkerConfig::default());
        assert!(chunker.next_chunk().expect("read").is_some());
        assert!(chunker.buf.len() <= INITIAL_BUFFER, "{}", chunker.buf.len());
    }

    #[test]
    fn reassembles_inputs_of_every_interesting_length() {
        let config = small();
        let lens = [
            0, 1, 63, 64, 65, 255, 256, 257, 1023, 1024, 1025, 2047, 2048, 2049, 5000, 12_345,
        ];
        for len in lens {
            let data = pseudo_random(len, len as u64 + 1);
            let chunks = collect(Cursor::new(data.clone()), config);
            assert_eq!(join(&chunks), data, "len {len}");
            check_invariants(&chunks, &config);
        }
    }

    #[test]
    fn reassembles_all_byte_values_and_runs() {
        let mut data: Vec<u8> = (0..=255).cycle().take(10_000).collect();
        data.extend_from_slice(&[0_u8; 5000]);
        data.extend_from_slice(&[0xff_u8; 5000]);
        let chunks = collect(Cursor::new(data.clone()), small());
        assert_eq!(join(&chunks), data);
        check_invariants(&chunks, &small());
    }

    #[test]
    fn read_sizes_do_not_change_boundaries() {
        let config = small();
        let data = pseudo_random(50_000, 11);
        let reference = lengths(&collect(Cursor::new(data.clone()), config));
        for step in [1, 2, 7, 63, 64, 100, 1000, 1024, 4096, 1 << 20] {
            let reader = Trickle {
                data: &data,
                position: 0,
                step,
            };
            assert_eq!(lengths(&collect(reader, config)), reference, "step {step}");
        }
    }

    #[test]
    fn matches_the_in_memory_splitter() {
        for (seed, config) in [small(), config(256, 1024, 4096), config(100, 700, 700)]
            .into_iter()
            .enumerate()
        {
            let data = pseudo_random(40_000, seed as u64 + 5);
            let streamed = lengths(&collect(Cursor::new(data.clone()), config));
            let sliced: Vec<usize> = config.split(&data).iter().map(|r| r.len()).collect();
            assert_eq!(streamed, sliced, "config {seed}");
        }
    }

    #[test]
    fn fixed_size_configuration_cuts_exactly_every_n_bytes() {
        let config = config(256, 256, 256);
        let data = pseudo_random(1000, 2);
        let chunks = collect(Cursor::new(data), config);
        assert_eq!(lengths(&chunks), [256, 256, 256, 232]);
        let offsets: Vec<u64> = chunks.iter().map(|chunk| chunk.offset).collect();
        assert_eq!(offsets, [0, 256, 512, 768]);
    }

    #[test]
    fn interrupted_reads_are_retried() {
        let config = small();
        let data = pseudo_random(20_000, 4);
        let reference = lengths(&collect(Cursor::new(data.clone()), config));
        let reader = Interrupting {
            inner: Trickle {
                data: &data,
                position: 0,
                step: 333,
            },
            interrupt_next: false,
        };
        assert_eq!(lengths(&collect(reader, config)), reference);
    }

    #[test]
    fn read_errors_surface_once_and_end_the_iteration() {
        let data = pseudo_random(20_000, 6);
        let reader = Failing {
            inner: Trickle {
                data: &data,
                position: 0,
                step: 500,
            },
            good: 5000,
        };
        let mut chunker = Chunker::new(reader, small());
        let mut delivered = 0;
        let err = loop {
            match chunker
                .next()
                .expect("the failure is reported before the end")
            {
                Ok(_) => delivered += 1,
                Err(err) => break err,
            }
        };
        assert!(
            delivered > 0,
            "chunks before the failure are still delivered"
        );
        assert!(err.to_string().contains("disk on fire"), "{err}");
        assert!(chunker.next().is_none());
        assert!(chunker.next().is_none());
    }

    #[test]
    fn memory_stays_bounded_on_large_streams() {
        let config = config(2048, 8192, 32_768);
        let total: u64 = 8 * 1024 * 1024;
        let mut chunker = Chunker::new(
            Endless {
                remaining: total,
                state: 0x1234_5678_9abc_def1,
            },
            config,
        );
        let mut seen = 0_u64;
        let mut chunks = 0_u64;
        let mut peak = 0;
        while let Some(chunk) = chunker.next_chunk().expect("read") {
            assert_eq!(chunk.offset, seen);
            assert!(chunk.data.len() <= config.max_size());
            seen += chunk.data.len() as u64;
            chunks += 1;
            peak = peak.max(chunker.buf.len());
        }
        assert_eq!(seen, total);
        assert!(chunks > 100, "{chunks}");
        assert!(peak <= 2 * config.max_size(), "buffer grew to {peak}");
    }

    #[test]
    fn average_chunk_size_is_close_to_the_target() {
        let config = config(2048, 8192, 32_768);
        let data = pseudo_random(4 * 1024 * 1024, 77);
        let chunks = collect(Cursor::new(data.clone()), config);
        let mean = data.len() as f64 / chunks.len() as f64;
        let target = config.avg_size() as f64;
        assert!(
            mean > 0.6 * target && mean < 1.8 * target,
            "mean chunk size {mean}"
        );
        let forced = chunks
            .iter()
            .filter(|c| c.data.len() == config.max_size())
            .count();
        assert!(
            forced * 100 < chunks.len(),
            "{forced} of {} hit the maximum",
            chunks.len()
        );
    }

    #[test]
    fn the_lending_api_reports_offsets_and_borrowed_data() {
        let config = small();
        let data = pseudo_random(5000, 8);
        let mut chunker = Chunker::new(Cursor::new(data.clone()), config);
        let mut rebuilt = Vec::new();
        while let Some(chunk) = chunker.next_chunk().expect("read") {
            assert_eq!(chunk.offset, rebuilt.len() as u64);
            rebuilt.extend_from_slice(chunk.data);
        }
        assert_eq!(rebuilt, data);
        assert!(chunker.next_chunk().expect("read").is_none());
    }

    #[test]
    fn identical_content_is_chunked_identically_regardless_of_context() {
        let config = small();
        let unit = pseudo_random(30_000, 21);
        let lone = collect(Cursor::new(unit.clone()), config);
        let mut doubled = unit.clone();
        doubled.extend_from_slice(&unit);
        let twice = collect(Cursor::new(doubled), config);
        // Every chunk of the first copy, except the one touching the join, is identical.
        let stable = lone.len() - 1;
        assert_eq!(lengths(&twice[..stable]), lengths(&lone[..stable]));
    }
}

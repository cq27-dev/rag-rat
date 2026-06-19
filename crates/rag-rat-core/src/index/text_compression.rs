//! Dictionary-trained zstd compression of stored chunk text (#77 Phase 2).
//!
//! One shared dictionary, trained once on a sample of the corpus and STORED IN THE DB, so the index
//! is self-contained: a copied or P2P-streamed index decompresses anywhere with no SQLite extension
//! and no per-connection setup. Per-chunk bulk one-shot compress/decompress keeps random-access
//! reads (one blob per row) instead of a single big stream.
//!
//! Why hand-rolled (not sqlite-zstd): the dictionary lives in the DB (self-contained); no extension
//! to load on every connection; works under ATTACH / streaming; full control. The dictionary is
//! essential — small chunks (~730 B avg) barely compress alone (~2x) but ~4x WITH a shared dict;
//! one global dict measured within ~3% of per-language, so we keep a single dict (no per-language
//! keying).

use anyhow::Result;

/// zstd level for stored chunk blobs. Compression runs at INDEX time on the write path, so the
/// level is chosen for write speed, NOT max ratio: measured on the real kernel corpus, level 3 =
/// 212 MB/s (~4.2x) vs level 19 = 4.1 MB/s (~4.9x) — 52x faster for ~80% of the ratio (levels
/// 15->22 buy almost nothing). Level 3 keeps a full reindex's added compression cost negligible
/// (the hard no-indexing-delay constraint); decompression throughput is level-independent.
pub(crate) const COMPRESSION_LEVEL: i32 = 3;

/// Default trained-dictionary size — zstd's own default; the #77 spike measured its ratios at this
/// size. Stored once in the DB, so its cost is negligible against the per-row savings.
pub(crate) const DEFAULT_DICT_SIZE: usize = 112 * 1024;

/// Train a single shared zstd dictionary from a sample of chunk texts (`max_size` caps the dict;
/// production passes [`DEFAULT_DICT_SIZE`]). The spike showed one global dict performs within ~3%
/// of per-language dicts, so we train ONE — no per-language keying / `lang` column needed.
pub(crate) fn train_dict(samples: &[Vec<u8>], max_size: usize) -> Result<Vec<u8>> {
    let refs: Vec<&[u8]> = samples.iter().map(Vec::as_slice).collect();
    Ok(zstd::dict::from_samples(&refs, max_size)?)
}

/// A compressor bound to the shared dictionary once, reused across many chunks (writes go through
/// this, not a per-call helper, which would re-prepare the dictionary every invocation — costly
/// over a full corpus). An empty dict means no-dictionary (plain zstd) — the fallback for corpora
/// too small to train on (`from_samples` hard-errors under ~7 samples); the read side recognizes
/// the same empty-dict sentinel, so write and read stay consistent.
pub(crate) struct ChunkCompressor<'a>(Option<zstd::bulk::Compressor<'a>>);

impl<'a> ChunkCompressor<'a> {
    pub(crate) fn new(dict: &'a [u8]) -> Result<Self> {
        Ok(Self(if dict.is_empty() {
            None
        } else {
            Some(zstd::bulk::Compressor::with_dictionary(COMPRESSION_LEVEL, dict)?)
        }))
    }

    pub(crate) fn compress(&mut self, text: &[u8]) -> Result<Vec<u8>> {
        match &mut self.0 {
            Some(compressor) => Ok(compressor.compress(text)?),
            None => Ok(zstd::bulk::compress(text, COMPRESSION_LEVEL)?),
        }
    }
}

/// Decompress one chunk blob. `capacity` is an upper bound on the decompressed size — store the
/// original byte length per row and pass it; a too-small value errors rather than truncating. An
/// empty `dict` means the blob was written without a dictionary (see [`ChunkCompressor`]).
///
/// Single-shot convenience — production read paths all reuse a [`ChunkDecompressor`] across a batch
/// (the per-call dictionary prep is the cost), so this now serves only round-trip tests (#77 Phase
/// 2).
#[cfg(test)]
pub(crate) fn decompress(blob: &[u8], dict: &[u8], capacity: usize) -> Result<Vec<u8>> {
    if dict.is_empty() {
        return Ok(zstd::bulk::decompress(blob, capacity)?);
    }
    let mut decompressor = zstd::bulk::Decompressor::with_dictionary(dict)?;
    Ok(decompressor.decompress(blob, capacity)?)
}

/// A decompressor bound to the shared dictionary once, reused across many chunks — the batch read
/// paths (lexical snippets, the embedding scan) decompress many blobs, and the per-call
/// [`decompress`] re-prepares the dictionary every time (~7x slower). An empty dict means
/// no-dictionary (plain zstd). `capacity` per call is the row's stored `raw_len`.
pub(crate) struct ChunkDecompressor<'a>(Option<zstd::bulk::Decompressor<'a>>);

impl<'a> ChunkDecompressor<'a> {
    pub(crate) fn new(dict: &'a [u8]) -> Result<Self> {
        Ok(Self(if dict.is_empty() {
            None
        } else {
            Some(zstd::bulk::Decompressor::with_dictionary(dict)?)
        }))
    }

    pub(crate) fn decompress(&mut self, blob: &[u8], capacity: usize) -> Result<Vec<u8>> {
        match &mut self.0 {
            Some(decompressor) => Ok(decompressor.decompress(blob, capacity)?),
            None => Ok(zstd::bulk::decompress(blob, capacity)?),
        }
    }
}

/// A chunk's stored text as fetched for a read (#77): the compressed `chunk_text` blob + `raw_len`,
/// with `chunks.text` as the fallback for a chunk not yet in the store (mid-migration / incremental
/// before a dict existed). [`resolve`] decompresses the blob (or returns the fallback) — the shared
/// shape every batch reader (lexical, graph local-context) collects per row before decompressing in
/// a post-loop, since decompress's `anyhow::Result` can't cross a rusqlite closure.
pub(crate) struct ChunkTextRow {
    pub(crate) fallback: String,
    pub(crate) blob: Option<Vec<u8>>,
    pub(crate) raw_len: Option<i64>,
}

impl ChunkTextRow {
    pub(crate) fn resolve(self, decompressor: &mut ChunkDecompressor) -> Result<String> {
        match (self.blob, self.raw_len) {
            (Some(blob), Some(raw_len)) =>
                Ok(String::from_utf8(decompressor.decompress(&blob, raw_len.max(0) as usize)?)?),
            _ => Ok(self.fallback),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A varied sample large enough to train a small dict (zstd wants samples >> dict size).
    fn sample() -> Vec<Vec<u8>> {
        (0..2000)
            .map(|i| {
                format!(
                    "pub fn handler_{i}(req: Request, ctx: &Ctx) -> Result<Response, Error> {{\n    \
                     let value = ctx.lookup({i})?;\n    Ok(Response::ok(value))\n}}\n"
                )
                .into_bytes()
            })
            .collect()
    }

    fn compress(text: &[u8], dict: &[u8]) -> Vec<u8> {
        ChunkCompressor::new(dict).unwrap().compress(text).unwrap()
    }

    #[test]
    fn round_trips_with_dict() {
        let dict = train_dict(&sample(), 16 * 1024).unwrap();
        let text =
            b"pub fn handler_x(req: Request, ctx: &Ctx) -> Result<Response, Error> { ok() }\n";
        let blob = compress(text, &dict);
        let back = decompress(&blob, &dict, text.len() + 64).unwrap();
        assert_eq!(back, text);
    }

    #[test]
    fn dict_beats_no_dict_on_a_small_chunk() {
        let samples = sample();
        let dict = train_dict(&samples, 16 * 1024).unwrap();
        let text = samples[0].as_slice();
        let with_dict = compress(text, &dict).len();
        let without_dict = zstd::bulk::compress(text, COMPRESSION_LEVEL).unwrap().len();
        assert!(
            with_dict < without_dict,
            "dict ({with_dict}B) should beat no-dict ({without_dict}B) on a small chunk"
        );
    }

    #[test]
    fn empty_text_round_trips() {
        let dict = train_dict(&sample(), 16 * 1024).unwrap();
        let blob = compress(b"", &dict);
        assert_eq!(decompress(&blob, &dict, 16).unwrap(), b"");
    }

    #[test]
    fn empty_dict_is_the_no_dict_fallback() {
        // A corpus too small to train on stores an empty dict; compress/decompress must round-trip
        // with no dictionary (plain zstd), so the tiny-repo fallback is transparent to callers.
        let text = b"fn tiny() -> u8 { 7 }\n";
        let blob = compress(text, &[]);
        assert_eq!(decompress(&blob, &[], text.len() + 16).unwrap(), text);
    }
}

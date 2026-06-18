//! Dictionary-trained zstd compression of stored chunk text (#77 Phase 2).
//!
//! One shared dictionary, trained once on a sample of the corpus and STORED IN THE DB, so the index
//! is self-contained: a copied or P2P-streamed index decompresses anywhere with no SQLite extension
//! and no per-connection setup. Per-chunk bulk one-shot compress/decompress keeps random-access
//! reads (one blob per row) instead of a single big stream.
//!
//! Why hand-rolled (not sqlite-zstd): the dictionary lives in the DB (self-contained); no extension
//! to load on every connection; works under ATTACH / streaming; full control. The #77 spike on real
//! chunk text measured ~3.9x (rust) / ~2.9x (ts) at the per-row level WITH a shared dict, vs only
//! ~2.3x / ~1.8x without — the dictionary is what makes small chunks (~730 B avg) compress, and one
//! global dict was within ~3% of per-language, so we keep a single dict (no per-language keying).

use anyhow::Result;

/// zstd level for stored chunk blobs. 19 is near-max ratio; chunks are small and compressed ONCE at
/// index time (amortized), while decompress on the read path stays ~GB/s.
pub(crate) const COMPRESSION_LEVEL: i32 = 19;

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

/// Compress one chunk's text with the shared dictionary.
pub(crate) fn compress(text: &[u8], dict: &[u8]) -> Result<Vec<u8>> {
    let mut compressor = zstd::bulk::Compressor::with_dictionary(COMPRESSION_LEVEL, dict)?;
    Ok(compressor.compress(text)?)
}

/// Decompress one chunk blob. `capacity` is an upper bound on the decompressed size — store the
/// original byte length per row and pass it; a too-small value errors rather than truncating.
pub(crate) fn decompress(blob: &[u8], dict: &[u8], capacity: usize) -> Result<Vec<u8>> {
    let mut decompressor = zstd::bulk::Decompressor::with_dictionary(dict)?;
    Ok(decompressor.decompress(blob, capacity)?)
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

    #[test]
    fn round_trips_with_dict() {
        let dict = train_dict(&sample(), 16 * 1024).unwrap();
        let text =
            b"pub fn handler_x(req: Request, ctx: &Ctx) -> Result<Response, Error> { ok() }\n";
        let blob = compress(text, &dict).unwrap();
        let back = decompress(&blob, &dict, text.len() + 64).unwrap();
        assert_eq!(back, text);
    }

    #[test]
    fn dict_beats_no_dict_on_a_small_chunk() {
        let samples = sample();
        let dict = train_dict(&samples, 16 * 1024).unwrap();
        let text = samples[0].as_slice();
        let with_dict = compress(text, &dict).unwrap().len();
        let without_dict = zstd::bulk::compress(text, COMPRESSION_LEVEL).unwrap().len();
        assert!(
            with_dict < without_dict,
            "dict ({with_dict}B) should beat no-dict ({without_dict}B) on a small chunk"
        );
    }

    #[test]
    fn empty_text_round_trips() {
        let dict = train_dict(&sample(), 16 * 1024).unwrap();
        let blob = compress(b"", &dict).unwrap();
        assert_eq!(decompress(&blob, &dict, 16).unwrap(), b"");
    }
}

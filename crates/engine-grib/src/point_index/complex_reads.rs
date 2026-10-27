//! Position fields in complex packing with spatial differencing, template
//! 5.3 (GFS), through their [`ComplexIndex`] (#1302): the CCSDS reads of
//! #1300 and #1301 over grid rows instead of RSIs.
//!
//! - From a whole message (the compressed-message cache, or one whole GET):
//!   the first read decodes every value once, indexing every row; later
//!   reads decode from the checkpoint of the first row they need.
//! - Warm, partial: ONE range GET from that checkpoint's data through the
//!   last needed value. The index holds the group descriptors, so the span
//!   is exact; a row past the indexed ones reads on from the last.
//! - Cold, partial: ONE prefix GET predicted from a sibling's index. Its
//!   descriptors then give the exact end of the last needed value, so a
//!   short prefix needs one exact continuation; a prefix that ends inside
//!   the descriptors reads on to a prediction from the header instead, and
//!   falls back to the whole message if that is still short.
//! - A bitmapped field places its needed values only through the bitmap,
//!   which precedes Section 7: its partial reads are prefixes, warm ones
//!   through the row after the last needed one, as the checkpoints bound it.

use super::*;
use crate::complex::ComplexIndex;

impl GribEngine {
    /// [`Self::points_from`] for a 5.3 message: `field`'s points from
    /// `bytes`, the whole message, through its index; the cached one when it
    /// has every row they read, else one built while decoding every value,
    /// replacing a partial one. Decodes the message in full when the index
    /// cannot read it.
    pub(super) fn complex_points_from(
        &self,
        index: &PointIndex,
        key: &MessageKey,
        field: &PointField<'_>,
        bytes: &[u8],
    ) -> Result<FieldRead, DataServerError> {
        let (entry, run, coords) = (field.entry, field.run, field.coords);
        let full = |error: PointError| self.decode_in_full(index, entry, bytes, error);
        let cached = match index.cache.get_untracked(key) {
            Some(Index::Complex(complex)) => Some(complex),
            _ => None,
        };
        if let Some(complex) = cached.as_ref().filter(|complex| complex.reaches(coords)) {
            index.cache.record_hit();
            return match complex.sample(bytes, coords) {
                Ok(values) => Ok(FieldRead::points(complex.metadata(), values)),
                Err(error) => full(error),
            };
        }
        let header = match complex::Header::parse(bytes) {
            Ok(header) => header,
            Err(error) => return full(error),
        };
        let mut sampled = None;
        let built = if cached.is_some() {
            // A partial index from an earlier prefix or range read.
            index.cache.record_miss();
            header.index(bytes, coords).map(|(complex, values)| {
                sampled = Some(values);
                index.store_complex(key, Arc::new(complex))
            })
        } else {
            index
                .cache
                .get_or_insert_with(key, || {
                    let (complex, values) = header.index(bytes, coords)?;
                    sampled = Some(values);
                    Ok::<_, PointError>(Index::Complex(Arc::new(complex)))
                })
                .and_then(|built| match built {
                    Index::Complex(complex) => Ok(complex),
                    Index::Ccsds(_) => Err(PointError::Invalid("a CCSDS index".into())),
                })
        };
        let complex = match built {
            Ok(complex) => complex,
            Err(error) => return full(error),
        };
        index.remember(entry.key(), true);
        index.note_sibling(entry.key(), run, &Index::Complex(complex.clone()));
        match sampled {
            Some(values) => Ok(FieldRead::points(complex.metadata(), values)),
            // Another reader built the index meanwhile.
            None => match complex.sample(bytes, coords) {
                Ok(values) => Ok(FieldRead::points(complex.metadata(), values)),
                Err(error) => full(error),
            },
        }
    }

    /// A 5.3 field whose message the point index holds an index of, from
    /// only the bytes its points need: one range GET of
    /// [`ComplexIndex::span`], or for a bitmapped field one prefix through
    /// [`ComplexIndex::prefix_bound`] (a prediction from the index itself
    /// when the checkpoints do not reach that far). The index counts a hit
    /// when it has every row the points read, a miss when the read goes on
    /// past them. `None` as [`Self::fetch_points`].
    pub(super) fn complex_warm(
        &self,
        index: &PointIndex,
        field: &PointField<'_>,
        path: &Path,
        key: &MessageKey,
        complex: Arc<ComplexIndex>,
    ) -> Result<Option<FieldRead>, DataServerError> {
        let (entry, coords) = (field.entry, field.coords);
        if complex.reaches(coords) {
            index.cache.record_hit();
        } else {
            index.cache.record_miss();
        }
        if complex.bitmapped() {
            let len = complex.header().message_len();
            let until = complex
                .prefix_bound(coords)
                .unwrap_or_else(|| complex.predict_prefix(coords, len, Kin::SameRun));
            let Some(prefix) = self.fetch_part(path, entry, 0..until, FetchMode::Prefix)? else {
                return Ok(None);
            };
            return self.complex_prefix(index, field, path, key, complex, prefix, false);
        }
        let span = match complex.span(coords, None) {
            Ok(span) => span,
            Err(error) => return unexpected(entry, error),
        };
        let bytes = if span.is_empty() {
            Bytes::new()
        } else {
            match self.fetch_part(path, entry, span.clone(), FetchMode::Range)? {
                Some(bytes) => bytes,
                None => return Ok(None),
            }
        };
        self.complex_read(index, field, key, complex, &bytes, span.start)
    }

    /// A 5.3 message not yet indexed, from `prefix`, its first bytes, which
    /// a GET predicted from a sibling's index fetched. Its header and group
    /// descriptors give an index without rows yet, and that index the exact
    /// span to read ([`Self::complex_prefix`]). A prefix that ends inside
    /// the descriptors reads on once to where the header predicts
    /// ([`complex::Header::predict_end`]). A layout this reader does not
    /// take reads the rest and decodes the message in full. `None` as
    /// [`Self::fetch_points`].
    pub(super) fn complex_cold(
        &self,
        index: &PointIndex,
        field: &PointField<'_>,
        path: &Path,
        key: &MessageKey,
        prefix: Bytes,
    ) -> Result<Option<FieldRead>, DataServerError> {
        let (entry, coords) = (field.entry, field.coords);
        let Some(len) = entry.length.and_then(|len| usize::try_from(len).ok()) else {
            return Ok(None);
        };
        let header = match complex::Header::parse_prefix(&prefix, len) {
            Ok(Some(header)) => header,
            Ok(None) => return Ok(None),
            // Another layout: read the rest and decode it in full.
            Err(PointError::Unsupported(_)) => {
                let whole = if prefix.len() < len {
                    let Some(rest) =
                        self.fetch_part(path, entry, prefix.len()..len, FetchMode::Continuation)?
                    else {
                        return Ok(None);
                    };
                    [&prefix[..], &rest[..]].concat()
                } else {
                    prefix.to_vec()
                };
                return self.points_from(index, key, field, &whole).map(Some);
            }
            Err(error) => return unexpected(entry, error),
        };
        index.cache.record_miss();
        let (bytes, continued) = match header.start_index(&prefix) {
            Some(_) => (prefix, false),
            None => {
                let end = header.predict_end(coords);
                let Some(more) =
                    self.fetch_part(path, entry, prefix.len()..end, FetchMode::Continuation)?
                else {
                    return Ok(None);
                };
                (Bytes::from([&prefix[..], &more[..]].concat()), true)
            }
        };
        let Some(complex) = header.start_index(&bytes) else {
            return Ok(None);
        };
        self.complex_prefix(index, field, path, key, Arc::new(complex), bytes, continued)
    }

    /// `field`'s points from `prefix`, the message's first bytes, through
    /// `complex`, an index of it: the bytes must hold its bitmap, if any,
    /// and reach the end of [`ComplexIndex::span`]; a prefix that falls
    /// short reads on to exactly there, unless it was `continued` already.
    #[allow(clippy::too_many_arguments)]
    fn complex_prefix(
        &self,
        index: &PointIndex,
        field: &PointField<'_>,
        path: &Path,
        key: &MessageKey,
        complex: Arc<ComplexIndex>,
        prefix: Bytes,
        continued: bool,
    ) -> Result<Option<FieldRead>, DataServerError> {
        let (entry, coords) = (field.entry, field.coords);
        let span = match complex
            .bitmap_in(&prefix, 0)
            .and_then(|bitmap| complex.span(coords, bitmap))
        {
            Ok(span) => span,
            Err(error) => return unexpected(entry, error),
        };
        let bytes = if span.end <= prefix.len() {
            prefix
        } else if continued {
            return Ok(None);
        } else {
            let Some(more) =
                self.fetch_part(path, entry, prefix.len()..span.end, FetchMode::Continuation)?
            else {
                return Ok(None);
            };
            Bytes::from([&prefix[..], &more[..]].concat())
        };
        self.complex_read(index, field, key, complex, &bytes, 0)
    }

    /// `field`'s points from `bytes`, the message's bytes from offset
    /// `start` on, through `complex`; the index the read extends is kept,
    /// and noted as its parameter's sibling in the run.
    fn complex_read(
        &self,
        index: &PointIndex,
        field: &PointField<'_>,
        key: &MessageKey,
        complex: Arc<ComplexIndex>,
        bytes: &[u8],
        start: usize,
    ) -> Result<Option<FieldRead>, DataServerError> {
        let (entry, run, coords) = (field.entry, field.run, field.coords);
        let (values, extended) = match complex.read(bytes, start, coords) {
            Ok(read) => read,
            Err(error) => return unexpected(entry, error),
        };
        let kept = index.store_complex(key, extended.map_or(complex, Arc::new));
        index.note_sibling(entry.key(), run, &Index::Complex(kept.clone()));
        index.remember(entry.key(), true);
        Ok(Some(FieldRead::points(kept.metadata(), values)))
    }
}

/// A partial read that went wrong: fatal errors fail the field, anything
/// else fetches the whole message.
fn unexpected(
    entry: &MessageEntry,
    error: PointError,
) -> Result<Option<FieldRead>, DataServerError> {
    match error {
        PointError::Fatal(error) => Err(error),
        error => {
            tracing::debug!(parameter = %entry.param, error = %error, "GRIB point field fetched whole");
            Ok(None)
        }
    }
}

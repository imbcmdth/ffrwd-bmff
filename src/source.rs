//! The reader everything that touches a file goes through.
//!
//! Two jobs. It counts, because how little of a file a reader has to
//! touch is a measurable claim and a claim nobody counts is a hope: a
//! keyframe scan that reads a twentieth of a file is worth saying so
//! only if something added up the bytes. And it is the one place in
//! the crate where a number that came out of a file becomes a buffer,
//! which turns "never allocate from an unchecked length" into a
//! property of one function rather than of every parser.
//!
//! Nothing here knows about boxes. A `moov` is read whole because it
//! is small and its children are walked in memory; an `mdat` is never
//! read at all, only the spans of it that a sample table named.

use std::io::{Read, Seek, SeekFrom};

use crate::{Error, Limits, Result};

/// What a read of a file cost.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    /// Bytes actually pulled out of the file.
    pub bytes_read: u64,
    /// Times the read head was moved somewhere it was not already.
    pub seeks: u64,
    /// How large the file is, which is what the bytes are a share of.
    pub len: u64,
}

impl Tally {
    /// Bytes read as a percentage of the file, for the line a scan
    /// prints when it is done.
    pub fn share(&self) -> f64 {
        if self.len == 0 {
            return 0.0;
        }
        self.bytes_read as f64 * 100.0 / self.len as f64
    }
}

/// A file, read by absolute position, with the cost kept.
#[derive(Debug)]
pub struct Source<R> {
    inner: R,
    len: u64,
    at: u64,
    bytes_read: u64,
    seeks: u64,
    limits: Limits,
}

impl<R: Read + Seek> Source<R> {
    /// A source over anything that reads and seeks, with the default
    /// [`Limits`]. The one seek this costs, to find the length, is
    /// counted like any other.
    pub fn new(inner: R) -> Result<Self> {
        Self::with_limits(inner, Limits::default())
    }

    /// The same, with a caller's own bounds.
    pub fn with_limits(mut inner: R, limits: Limits) -> Result<Self> {
        let len = inner.seek(SeekFrom::End(0))?;
        inner.seek(SeekFrom::Start(0))?;
        Ok(Self {
            inner,
            len,
            at: 0,
            bytes_read: 0,
            seeks: 1,
            limits,
        })
    }

    /// How many bytes the file holds.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The bounds every read through this source is held to.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// What the reading has cost so far.
    pub fn tally(&self) -> Tally {
        Tally {
            bytes_read: self.bytes_read,
            seeks: self.seeks,
            len: self.len,
        }
    }

    /// Forgets the cost so far, so a caller can time one phase.
    pub fn reset_tally(&mut self) {
        self.bytes_read = 0;
        self.seeks = 0;
    }

    /// Up to `want` bytes at `at`, short at the end of the file.
    ///
    /// The only allocation in this crate that comes from a length a
    /// file chose, and it is clamped twice first: to the bytes that
    /// actually remain, and to [`Limits::max_read`]. A `want` past the
    /// ceiling is a refusal rather than a silent truncation, because a
    /// caller that asked for a gigabyte has read a length it should not
    /// trust.
    pub fn read_at(&mut self, at: u64, want: usize) -> Result<Vec<u8>> {
        if want > self.limits.max_read {
            return Err(Error::format("a value larger than this reader will hold").at(at));
        }
        if at >= self.len || want == 0 {
            return Ok(Vec::new());
        }
        let left = usize::try_from(self.len - at).unwrap_or(usize::MAX);
        let take = want.min(left);
        if self.at != at {
            self.inner.seek(SeekFrom::Start(at))?;
            self.seeks += 1;
            self.at = at;
        }
        let mut out = vec![0u8; take];
        self.inner.read_exact(&mut out)?;
        self.at += take as u64;
        self.bytes_read += take as u64;
        Ok(out)
    }

    /// Exactly `want` bytes at `at`. Anything less is a truncation.
    pub fn exact_at(&mut self, at: u64, want: usize) -> Result<Vec<u8>> {
        let got = self.read_at(at, want)?;
        if got.len() != want {
            return Err(Error::truncated("the file ends inside a value").at(at));
        }
        Ok(got)
    }

    /// The bytes of a span, refused when the span is not inside the
    /// file or is larger than [`Limits::max_read`].
    pub fn span(&mut self, at: u64, len: u64) -> Result<Vec<u8>> {
        let len =
            usize::try_from(len).map_err(|_| Error::format("a span wider than memory").at(at))?;
        self.exact_at(at, len)
    }

    /// The reader back, for a caller that has to write as well.
    pub fn into_inner(self) -> R {
        self.inner
    }

    /// The reader, borrowed. Reads made through it are not counted, so
    /// this is for writing and truncating, not for scanning.
    pub fn inner_mut(&mut self) -> &mut R {
        self.at = u64::MAX;
        &mut self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn source(bytes: &[u8]) -> Source<Cursor<Vec<u8>>> {
        Source::new(Cursor::new(bytes.to_vec())).expect("a source")
    }

    #[test]
    fn a_read_is_clamped_to_the_file_and_counted() {
        let mut src = source(&[1, 2, 3, 4, 5, 6, 7, 8]);
        src.reset_tally();
        assert_eq!(src.read_at(4, 100).expect("bytes"), vec![5, 6, 7, 8]);
        assert_eq!(src.tally().bytes_read, 4);
        assert_eq!(src.tally().seeks, 1, "one move of the head");
        // Reading on from where the head already is costs no seek.
        assert!(src.read_at(8, 4).expect("bytes").is_empty());
        assert_eq!(src.tally().seeks, 1);
        assert!(src.exact_at(4, 5).is_err(), "a short read is a truncation");
    }

    #[test]
    fn a_length_past_the_ceiling_is_refused_rather_than_allocated() {
        let mut src = source(&[0; 16]);
        assert!(src.read_at(0, crate::MAX_READ + 1).is_err());
        assert!(src.span(0, u64::MAX).is_err());
    }

    #[test]
    fn the_ceiling_is_a_default_a_caller_can_move() {
        let limits = Limits {
            max_read: 4,
            ..Limits::default()
        };
        let mut src = Source::with_limits(Cursor::new(vec![7u8; 16]), limits).expect("a source");
        assert_eq!(src.read_at(0, 4).expect("bytes").len(), 4);
        let err = src.read_at(0, 5).expect_err("a refusal");
        assert_eq!(err.offset(), Some(0));
    }
}

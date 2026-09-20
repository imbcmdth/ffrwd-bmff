//! ISO base media files, read and written: MP4, MOV, 3GP, and the
//! fragmented shape of the same boxes.
//!
//! One box-walking layer serves four callers that used to walk boxes
//! separately. A **reader** over [`Read`] and [`Seek`] that finds a
//! track and every sample of it without pulling a picture into memory
//! ([`source`], [`track`], [`fragment`]). A **scanner** that cuts a
//! byte stream arriving a chunk at a time into an init segment and one
//! fragment per `moof` ([`scanner`]). A **writer** that builds the
//! `ftyp`+`moov` a decoder needs and the `moof`+`mdat` fragments after
//! it ([`mux`]). And a **patcher** that finds, appends or replaces a
//! top-level box in place, for a file that has to carry something that
//! is not media ([`patch`]).
//!
//! Three properties shape all of it.
//!
//! **It is parsing strangers' files.** No buffer is ever sized from a
//! number a file chose without that number being checked against the
//! bytes that actually exist first. [`Source::read_at`] is the only
//! place in the crate where a length becomes a file read, and
//! [`boxes::Bytes::count`] is the only place a table header becomes a
//! `Vec`. Truncations and random bytes come back as [`Error`], never
//! as a panic and never as a gigabyte of zeroes. The limits are in
//! [`Limits`] and every one of them is a default a caller can change.
//!
//! **It knows nothing about codecs.** A decoder configuration record
//! (`avcC`, `hvcC`, `av1C`, an `esds` payload) is an opaque byte slice
//! this crate carries from a sample entry to a caller, or from a caller
//! into one. Parsing it, and the NAL units or OBUs of a sample, is
//! `ffrwd-nal`'s job, and neither crate depends on the other.
//!
//! **It has no dependencies.** It builds for `wasm32-wasip2` and for
//! the host, so a wasm module compiles it in and its tests run without
//! one.
//!
//! [`Read`]: std::io::Read
//! [`Seek`]: std::io::Seek
//! [`Source::read_at`]: source::Source::read_at

#![forbid(unsafe_code)]

use core::fmt;

pub mod boxes;
pub mod fragment;
pub mod mux;
pub mod patch;
pub mod scanner;
pub mod source;
pub mod time;
pub mod track;

pub use boxes::{boxed, full_boxed, uuid_boxed, BoxHeader, BoxSize, BoxSpan, Bytes, Child};
pub use source::{Source, Tally};
pub use time::{ms, rescale, ticks_to_micros};
pub use track::{Edit, Handler, Pick, Sample, SampleEntry, Track, TrexDefaults};

// ------------------------------------------------------------------ //
// What went wrong, and where.
// ------------------------------------------------------------------ //

/// What went wrong, said without allocating.
///
/// Every message is a `&'static str`, so an error costs nothing to
/// build on a path that is already going badly, which matters when the
/// path in question is a truncation test running the whole parser once
/// for every byte of a file. The offset is the file or stream position
/// the trouble was found at, where the parser knew it, and the box
/// kind is the four characters of the box it was inside.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Fault {
    /// What the rule was.
    pub what: &'static str,
    /// Where in the file or stream, where that is known.
    pub at: Option<u64>,
    /// Which box, where that is known.
    pub kind: Option<[u8; 4]>,
}

impl Fault {
    pub const fn new(what: &'static str) -> Self {
        Self {
            what,
            at: None,
            kind: None,
        }
    }
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.what)?;
        if let Some(kind) = self.kind {
            f.write_str(" in '")?;
            for byte in kind {
                let shown = if byte.is_ascii_graphic() {
                    byte as char
                } else {
                    '.'
                };
                fmt::Write::write_char(f, shown)?;
            }
            f.write_str("'")?;
        }
        if let Some(at) = self.at {
            write!(f, " at byte {at}")?;
        }
        Ok(())
    }
}

/// The one error this crate returns.
///
/// The three kinds are worth keeping apart: a [`Truncated`] stream may
/// be complete later, a [`Format`] one never will be, and an
/// [`Unsupported`] one is well formed and simply not something this
/// crate carries.
///
/// [`Truncated`]: Error::Truncated
/// [`Format`]: Error::Format
/// [`Unsupported`]: Error::Unsupported
#[derive(Debug)]
pub enum Error {
    /// The file would not read.
    Io(std::io::Error),
    /// The bytes are not the shape the format's own rules require.
    Format(Fault),
    /// The bytes end inside something that is not finished.
    Truncated(Fault),
    /// A well-formed shape this crate does not carry, said rather than
    /// guessed at.
    Unsupported(Fault),
}

impl Error {
    pub const fn format(what: &'static str) -> Self {
        Error::Format(Fault::new(what))
    }

    pub const fn truncated(what: &'static str) -> Self {
        Error::Truncated(Fault::new(what))
    }

    pub const fn unsupported(what: &'static str) -> Self {
        Error::Unsupported(Fault::new(what))
    }

    /// The offset, filled in only when the error does not carry one
    /// already: the innermost parser that knew where it was wins.
    pub fn at(mut self, at: u64) -> Self {
        if let Some(fault) = self.fault_mut() {
            if fault.at.is_none() {
                fault.at = Some(at);
            }
        }
        self
    }

    /// The box the trouble was inside, filled in the same way.
    pub fn in_box(mut self, kind: [u8; 4]) -> Self {
        if let Some(fault) = self.fault_mut() {
            if fault.kind.is_none() {
                fault.kind = Some(kind);
            }
        }
        self
    }

    /// Where the trouble was, where the parser knew.
    pub fn offset(&self) -> Option<u64> {
        self.fault().and_then(|fault| fault.at)
    }

    /// The fault behind the error, for everything but an I/O failure.
    pub fn fault(&self) -> Option<&Fault> {
        match self {
            Error::Io(_) => None,
            Error::Format(fault) | Error::Truncated(fault) | Error::Unsupported(fault) => {
                Some(fault)
            }
        }
    }

    fn fault_mut(&mut self) -> Option<&mut Fault> {
        match self {
            Error::Io(_) => None,
            Error::Format(fault) | Error::Truncated(fault) | Error::Unsupported(fault) => {
                Some(fault)
            }
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(err) => write!(f, "{err}"),
            Error::Format(fault) | Error::Truncated(fault) | Error::Unsupported(fault) => {
                write!(f, "{fault}")
            }
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(err: std::io::Error) -> Self {
        Error::Io(err)
    }
}

/// The crate's result.
pub type Result<T> = core::result::Result<T, Error>;

// ------------------------------------------------------------------ //
// The bounds.
// ------------------------------------------------------------------ //

/// The most bytes one read will hand back.
///
/// A `moov` of a long film is a few megabytes and a sample table of
/// thirty thousand entries is far less; sixty-four megabytes is well
/// past both and well short of a length a damaged file could use to
/// exhaust a reader.
pub const MAX_READ: usize = 64 << 20;

/// The most samples a track may have before a read refuses it. Twenty
/// million is about six days at thirty frames a second.
pub const MAX_SAMPLES: usize = 20_000_000;

/// The most top-level boxes a file may have.
pub const MAX_TOP_LEVEL: usize = 1 << 20;

/// The most child boxes one box's body may hold.
pub const MAX_CHILDREN: usize = 1 << 16;

/// The most bytes the streaming scanner will hold for one box.
pub const MAX_BUFFERED_BOX: u64 = 64 << 20;

/// Every bound in the crate, as a default a caller may change.
///
/// None of these is in the file format. They are the line between a
/// file that is merely large and a file whose numbers are a way of
/// making a reader allocate: a `stsz` claiming four billion samples is
/// refused here rather than believed and then fed to a `Vec`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// The ceiling on one [`Source::read_at`](source::Source::read_at).
    pub max_read: usize,
    /// The ceiling on a track's sample count.
    pub max_samples: usize,
    /// The ceiling on a file's top-level box count.
    pub max_top_level: usize,
    /// The ceiling on one box's child count.
    pub max_children: usize,
    /// The ceiling on a box the streaming scanner must hold whole.
    pub max_buffered_box: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_read: MAX_READ,
            max_samples: MAX_SAMPLES,
            max_top_level: MAX_TOP_LEVEL,
            max_children: MAX_CHILDREN,
            max_buffered_box: MAX_BUFFERED_BOX,
        }
    }
}

/// A `Vec` grown to `count` items, or an error rather than an abort.
///
/// `count` has already been checked against the bytes that exist by
/// the time this is called; this is the second belt, for an allocator
/// that cannot honour a reservation the file could in principle
/// justify.
pub(crate) fn reserved<T>(count: usize, what: &'static str) -> Result<Vec<T>> {
    let mut out: Vec<T> = Vec::new();
    out.try_reserve(count)
        .map_err(|_| Error::format(what))
        .map(|()| out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_error_keeps_the_innermost_offset_and_says_where_it_was() {
        let err = Error::format("a table longer than the box holding it")
            .at(12)
            .in_box(*b"trun")
            .at(99)
            .in_box(*b"moof");
        assert_eq!(err.offset(), Some(12), "the inner offset wins");
        assert_eq!(
            err.to_string(),
            "a table longer than the box holding it in 'trun' at byte 12"
        );
        // An I/O error carries no fault of ours to fill in.
        let io = Error::from(std::io::Error::other("gone")).at(5);
        assert_eq!(io.offset(), None);
    }

    #[test]
    fn a_reservation_larger_than_memory_is_an_error_and_not_an_abort() {
        assert!(reserved::<u64>(usize::MAX / 8, "too much").is_err());
        assert!(reserved::<u8>(4, "fine").expect("a vec").capacity() >= 4);
    }
}

//! Cutting an arriving byte stream into segments, a chunk at a time.
//!
//! Feeds on an arbitrary byte stream (a pipe, a socket, a file read
//! forwards) and cuts it at top-level box boundaries: one **init
//! segment**, everything before the first `moof` and including a
//! `moov`, then one **fragment** per `moof` and the boxes through its
//! `mdat`. Each `moof` is read far enough to say its `mfhd` sequence
//! number (§8.8.5) and whether its first sample is a sync sample
//! (§8.8.7, §8.8.8), so a caller can cut groups on real keyframes
//! rather than assume them.
//!
//! Trailing boxes after the last fragment, which for ffmpeg means the
//! `mfra` it writes even into a pipe (§8.8.9), are collected aside and
//! not emitted: a live subscriber has no use for a random access index
//! of something it is watching go past.
//!
//! **What it will hold.** Unlike the reader, a scanner has no file to
//! seek in: a box it must emit, `mdat` included, is a box it has to
//! keep in memory until it is whole. That is a real bound and it is
//! [`Limits::max_buffered_box`], which is a default rather than a law:
//! a stream of one-second fragments at a high bitrate is a different
//! number from a stream of one frame each. The old copy of this code
//! called any box past a quarter of a gigabyte malformed, which is a
//! statement about a file rather than about the reader, and it is
//! gone.
//!
//! **A box of declared size zero** (§4.2) runs to the end of the
//! stream. That is exactly what some muxers write for the `mdat` of a
//! stream going into a pipe, where the length is not known when the
//! header is written. It is accepted: the box stays open, its bytes
//! accumulate, and [`Scanner::finish`] closes it and emits what it
//! held. Nothing after it can exist, so a box arriving after one is a
//! malformed stream.
//!
//! No I/O: [`Scanner::push`] takes bytes, [`Scanner::poll`] yields
//! segments, [`Scanner::finish`] says whether the stream ended on a
//! clean boundary.

use std::collections::VecDeque;

use crate::boxes::{self, Bytes, Child};
use crate::fragment::NON_SYNC;
use crate::{Error, Limits, Result};

/// One cut of the stream.
#[derive(Clone, Debug)]
pub enum Segment {
    /// Everything before the first `moof`: `ftyp`, `moov`, and any
    /// boxes between them. A late subscriber needs exactly these bytes
    /// before any fragment.
    Init(Init),
    /// One `moof` and the boxes through its `mdat`.
    Fragment(Fragment),
}

/// The init segment.
#[derive(Clone, Debug)]
pub struct Init {
    /// The segment bytes, verbatim.
    pub bytes: Vec<u8>,
    /// Absolute stream offset of the first byte.
    pub offset: u64,
}

/// One movie fragment (§8.8.4).
#[derive(Clone, Debug)]
pub struct Fragment {
    /// The fragment bytes, `moof` through `mdat`, verbatim.
    pub bytes: Vec<u8>,
    /// Absolute stream offset of the first byte, which is also the
    /// number [`Track::fragment_samples_at`] wants.
    ///
    /// [`Track::fragment_samples_at`]: crate::track::Track::fragment_samples_at
    pub offset: u64,
    /// The `mfhd` sequence number, when the fragment carries one
    /// (§8.8.5).
    pub sequence: Option<u32>,
    /// Whether the first sample is a sync sample. `None` when the
    /// sample flags live only in the `moov`'s `trex` defaults, which
    /// this scanner does not track.
    pub keyframe: Option<bool>,
}

enum Phase {
    /// Accumulating boxes before the first `moof`.
    Init,
    /// Cutting `moof`..`mdat` fragments.
    Fragments,
    /// Accumulating trailing boxes after the last fragment.
    Trailer,
}

/// Which pile the bytes of a box with no declared length go on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OpenEnded {
    None,
    Init,
    Fragment,
    Trailer,
}

/// The incremental segmenter.
pub struct Scanner {
    buf: Vec<u8>,
    /// Absolute stream offset of `buf[0]`.
    base: u64,
    phase: Phase,
    out: VecDeque<Segment>,
    init: Vec<u8>,
    saw_moov: bool,
    /// The fragment being collected; starts with its `moof`.
    frag: Vec<u8>,
    frag_start: u64,
    /// Length of the current fragment's `moof` box within `frag`.
    frag_moof_len: usize,
    trailer: Vec<u8>,
    /// Set once a box of declared size zero has opened: everything
    /// from then on belongs to it.
    open_ended: OpenEnded,
    limits: Limits,
}

impl Scanner {
    /// A scanner with the default bounds.
    pub fn new() -> Self {
        Self::with_limits(Limits::default())
    }

    /// A scanner with a caller's own bounds.
    pub fn with_limits(limits: Limits) -> Self {
        Self {
            buf: Vec::new(),
            base: 0,
            phase: Phase::Init,
            out: VecDeque::new(),
            init: Vec::new(),
            saw_moov: false,
            frag: Vec::new(),
            frag_start: 0,
            frag_moof_len: 0,
            trailer: Vec::new(),
            open_ended: OpenEnded::None,
            limits,
        }
    }

    /// Feeds more stream bytes; complete segments queue for
    /// [`poll`](Self::poll).
    pub fn push(&mut self, chunk: &[u8]) -> Result<()> {
        match self.open_ended {
            OpenEnded::None => {
                self.buf.extend_from_slice(chunk);
                self.scan()
            }
            OpenEnded::Init => {
                self.init.extend_from_slice(chunk);
                self.base += chunk.len() as u64;
                self.bound(self.init.len())
            }
            OpenEnded::Fragment => {
                self.frag.extend_from_slice(chunk);
                self.base += chunk.len() as u64;
                self.bound(self.frag.len())
            }
            OpenEnded::Trailer => {
                self.trailer.extend_from_slice(chunk);
                self.base += chunk.len() as u64;
                Ok(())
            }
        }
    }

    /// The next complete segment, if one is ready.
    pub fn poll(&mut self) -> Option<Segment> {
        self.out.pop_front()
    }

    /// Declares end of stream.
    ///
    /// A box of declared size zero ends here, and what it held is
    /// emitted. Anything else in flight is a truncation: a partial box
    /// header, a `moof` with no `mdat` after it, or a stream that
    /// never reached a fragment at all.
    pub fn finish(&mut self) -> Result<()> {
        match self.open_ended {
            OpenEnded::Fragment => {
                // §4.2's open-ended box closes at the end of the
                // stream, so the fragment it was part of is whole now.
                self.close_fragment()?;
                self.open_ended = OpenEnded::None;
                return Ok(());
            }
            OpenEnded::Init | OpenEnded::Trailer => {
                return Err(Error::truncated(
                    "the stream ended inside an open-ended box before any fragment",
                )
                .at(self.base))
            }
            OpenEnded::None => {}
        }
        if !self.frag.is_empty() {
            return Err(
                Error::truncated("the stream ended inside a fragment, with no mdat")
                    .at(self.frag_start),
            );
        }
        if !self.buf.is_empty() {
            return Err(Error::truncated("the stream ended inside a box").at(self.base));
        }
        if matches!(self.phase, Phase::Init) {
            return Err(
                Error::truncated("the stream ended before any movie fragment").at(self.base),
            );
        }
        Ok(())
    }

    /// Cumulative count of bytes fully consumed into segments or the
    /// trailer. Bytes still buffered are not counted.
    pub fn consumed(&self) -> u64 {
        self.base
    }

    /// Trailing boxes after the last fragment, so far: `mfra` and kin.
    /// Not part of any segment, because a live subscriber cannot use
    /// them.
    pub fn trailer(&self) -> &[u8] {
        &self.trailer
    }

    fn bound(&self, held: usize) -> Result<()> {
        if held as u64 > self.limits.max_buffered_box {
            return Err(
                Error::unsupported("a box larger than this scanner will hold in memory")
                    .at(self.base),
            );
        }
        Ok(())
    }

    /// Cuts as many complete boxes off the head of `buf` as it holds.
    fn scan(&mut self) -> Result<()> {
        loop {
            let Some(header) = boxes::parse_header(&self.buf, self.base)? else {
                return Ok(());
            };
            let open_ended = header.size == crate::BoxSize::ToEnd;
            let size = match header.size {
                crate::BoxSize::Exact(size) => {
                    if size > self.limits.max_buffered_box {
                        return Err(Error::unsupported(
                            "a box larger than this scanner will hold in memory",
                        )
                        .at(self.base)
                        .in_box(header.kind));
                    }
                    size as usize
                }
                // Everything left, and everything still to come.
                crate::BoxSize::ToEnd => self.buf.len(),
            };

            match self.phase {
                Phase::Init => {
                    if header.is(b"moof") {
                        if !self.saw_moov {
                            return Err(Error::format("a moof before any moov").at(self.base));
                        }
                        let offset = self.base - self.init.len() as u64;
                        let bytes = std::mem::take(&mut self.init);
                        self.out.push_back(Segment::Init(Init { bytes, offset }));
                        self.phase = Phase::Fragments;
                        // The moof itself is handled by the next pass.
                        continue;
                    }
                    if !open_ended && self.buf.len() < size {
                        return Ok(());
                    }
                    if header.is(b"moov") {
                        self.saw_moov = true;
                    }
                    self.init.extend_from_slice(&self.buf[..size]);
                    self.consume(size);
                    if open_ended {
                        self.open_ended = OpenEnded::Init;
                        return self.bound(self.init.len());
                    }
                }
                Phase::Fragments => {
                    if self.frag.is_empty() && !header.is(b"moof") {
                        // The stream's trailer has begun (ffmpeg: mfra).
                        self.phase = Phase::Trailer;
                        continue;
                    }
                    if !self.frag.is_empty() && header.is(b"moof") {
                        return Err(Error::format("a moof before the previous fragment's mdat")
                            .at(self.base));
                    }
                    if !open_ended && self.buf.len() < size {
                        return Ok(());
                    }
                    if self.frag.is_empty() {
                        self.frag_start = self.base;
                        self.frag_moof_len = size;
                    }
                    self.frag.extend_from_slice(&self.buf[..size]);
                    self.consume(size);
                    self.bound(self.frag.len())?;
                    if open_ended {
                        // An mdat that runs to the end of the stream:
                        // the fragment closes when the stream does.
                        self.open_ended = OpenEnded::Fragment;
                        return Ok(());
                    }
                    if header.is(b"mdat") {
                        self.close_fragment()?;
                    }
                }
                Phase::Trailer => {
                    if header.is(b"moof") {
                        return Err(
                            Error::format("a moof after the stream's trailer began").at(self.base)
                        );
                    }
                    if !open_ended && self.buf.len() < size {
                        return Ok(());
                    }
                    self.trailer.extend_from_slice(&self.buf[..size]);
                    self.consume(size);
                    if open_ended {
                        self.open_ended = OpenEnded::Trailer;
                        return Ok(());
                    }
                }
            }
        }
    }

    fn close_fragment(&mut self) -> Result<()> {
        if self.frag.is_empty() {
            return Err(Error::truncated("a fragment with no moof").at(self.frag_start));
        }
        let summary = moof_summary(&self.frag[..self.frag_moof_len], self.frag_start)?;
        let fragment = Fragment {
            bytes: std::mem::take(&mut self.frag),
            offset: self.frag_start,
            sequence: summary.sequence,
            keyframe: summary.first_sample_keyframe,
        };
        self.frag_moof_len = 0;
        self.out.push_back(Segment::Fragment(fragment));
        Ok(())
    }

    fn consume(&mut self, n: usize) {
        self.buf.drain(..n);
        self.base += n as u64;
    }
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new()
    }
}

/// What one `moof` says about its fragment, without reading its
/// samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MoofSummary {
    /// The `mfhd` sequence number (§8.8.5).
    pub sequence: Option<u32>,
    /// Whether the first sample of the first track fragment that
    /// declares sample flags is a sync sample. `None` when neither a
    /// `tfhd` default nor a `trun` decides it, which is the case where
    /// the answer is in the `moov`'s `trex`.
    pub first_sample_keyframe: Option<bool>,
}

/// A complete `moof` box, header included, read for its summary.
///
/// The first sample's flags follow §8.8.8's precedence: the `trun`'s
/// `first_sample_flags` when present, else the first sample's own
/// per-sample flags, else the `tfhd` default (§8.8.7). Only the first
/// `traf` that yields an answer is consulted.
pub fn moof_summary(moof: &[u8], at: u64) -> Result<MoofSummary> {
    let header = boxes::parse_header(moof, at)?
        .ok_or_else(|| Error::truncated("a moof shorter than a box header").at(at))?;
    let head = usize::try_from(header.header_len).unwrap_or(usize::MAX);
    let body = moof
        .get(head..)
        .ok_or_else(|| Error::truncated("a moof shorter than its own header").at(at))?;
    let mut sequence = None;
    let mut keyframe = None;
    for child in boxes::children(body, at + header.header_len)? {
        match &child.kind {
            b"mfhd" => {
                let mut bytes = Bytes::at(child.body, child.at);
                bytes.full_box()?;
                sequence = Some(bytes.u32()?);
            }
            b"traf" if keyframe.is_none() => {
                if let Some(flags) = traf_first_sample_flags(&child)? {
                    keyframe = Some(flags & NON_SYNC == 0);
                }
            }
            _ => {}
        }
    }
    Ok(MoofSummary {
        sequence,
        first_sample_keyframe: keyframe,
    })
}

/// The first sample's flags within one `traf`, per §8.8.8's
/// precedence.
fn traf_first_sample_flags(traf: &Child<'_>) -> Result<Option<u32>> {
    let mut default = None;
    for child in boxes::children(traf.body, traf.at)? {
        match &child.kind {
            b"tfhd" => {
                default = crate::fragment::read_tfhd(child.body, child.at)?.default_flags;
            }
            b"trun" => {
                let mut bytes = Bytes::at(child.body, child.at);
                let (_, flags) = bytes.full_box()?;
                let count = bytes.u32()?;
                if flags & 0x01 != 0 {
                    bytes.skip(4)?; // data_offset
                }
                if flags & 0x04 != 0 {
                    return Ok(Some(bytes.u32()?)); // first_sample_flags
                }
                if flags & 0x400 != 0 && count > 0 {
                    if flags & 0x100 != 0 {
                        bytes.skip(4)?; // sample_duration
                    }
                    if flags & 0x200 != 0 {
                        bytes.skip(4)?; // sample_size
                    }
                    return Ok(Some(bytes.u32()?));
                }
                // The first run decides; without flags of its own the
                // tfhd default, or nothing, stands.
                return Ok(default);
            }
            _ => {}
        }
    }
    Ok(default)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxes::{boxed, full_boxed};

    fn moof_bytes(sequence: u32, keyframe: bool) -> Vec<u8> {
        let flags: u32 = if keyframe { 0x0200_0000 } else { 0x0101_0000 };
        let mut trun_payload = 1u32.to_be_bytes().to_vec();
        trun_payload.extend_from_slice(&0i32.to_be_bytes()); // data_offset
        trun_payload.extend_from_slice(&flags.to_be_bytes()); // first_sample_flags
        trun_payload.extend_from_slice(&4u32.to_be_bytes()); // sample_size
        let traf = boxed(
            b"traf",
            &[
                full_boxed(b"tfhd", 0, 0x02_0000, &1u32.to_be_bytes()),
                full_boxed(b"trun", 1, 0x01 | 0x04 | 0x200, &trun_payload),
            ]
            .concat(),
        );
        boxed(
            b"moof",
            &[full_boxed(b"mfhd", 0, 0, &sequence.to_be_bytes()), traf].concat(),
        )
    }

    fn stream() -> Vec<u8> {
        let mut out = boxed(b"ftyp", b"iso5");
        out.extend_from_slice(&boxed(b"moov", &[0; 16]));
        for index in 0..3u32 {
            out.extend_from_slice(&moof_bytes(index + 1, index == 0));
            out.extend_from_slice(&boxed(b"mdat", &[index as u8; 4]));
        }
        out.extend_from_slice(&boxed(b"mfra", &[0; 8]));
        out
    }

    fn cut(bytes: &[u8], step: usize) -> Vec<Segment> {
        let mut scanner = Scanner::new();
        let mut out = Vec::new();
        for chunk in bytes.chunks(step.max(1)) {
            scanner.push(chunk).expect("a push");
            while let Some(segment) = scanner.poll() {
                out.push(segment);
            }
        }
        scanner.finish().expect("a clean end");
        while let Some(segment) = scanner.poll() {
            out.push(segment);
        }
        out
    }

    #[test]
    fn a_stream_cuts_the_same_way_whatever_size_the_chunks_are() {
        let bytes = stream();
        for step in [1usize, 3, 7, 64, 4096] {
            let segments = cut(&bytes, step);
            assert_eq!(segments.len(), 4, "step {step}");
            match &segments[0] {
                Segment::Init(init) => {
                    assert_eq!(init.offset, 0);
                    assert_eq!(init.bytes.len(), 12 + 24);
                }
                other => panic!("step {step}: {other:?}"),
            }
            let keyframes: Vec<Option<bool>> = segments[1..]
                .iter()
                .map(|segment| match segment {
                    Segment::Fragment(frag) => frag.keyframe,
                    other => panic!("{other:?}"),
                })
                .collect();
            assert_eq!(keyframes, [Some(true), Some(false), Some(false)]);
        }
    }

    #[test]
    fn the_trailer_is_kept_aside_and_not_emitted() {
        let bytes = stream();
        let mut scanner = Scanner::new();
        scanner.push(&bytes).expect("a push");
        scanner.finish().expect("a clean end");
        let mut count = 0;
        while scanner.poll().is_some() {
            count += 1;
        }
        assert_eq!(count, 4);
        assert_eq!(&scanner.trailer()[4..8], b"mfra");
    }

    #[test]
    fn an_mdat_of_declared_size_zero_closes_at_the_end_of_the_stream() {
        // §4.2's third size, which a muxer writing into a pipe uses
        // because it does not know the length when it writes the
        // header. The old copy of this code called it unsupported.
        let mut bytes = boxed(b"ftyp", b"iso5");
        bytes.extend_from_slice(&boxed(b"moov", &[0; 16]));
        bytes.extend_from_slice(&moof_bytes(1, true));
        bytes.extend_from_slice(&[0, 0, 0, 0]);
        bytes.extend_from_slice(b"mdat");
        bytes.extend_from_slice(&[7; 40]);

        let mut scanner = Scanner::new();
        scanner.push(&bytes).expect("a push");
        assert!(
            matches!(scanner.poll(), Some(Segment::Init(_))),
            "the init segment leaves before the open box closes"
        );
        assert!(scanner.poll().is_none(), "the fragment is still open");
        scanner.finish().expect("the open box closes at the end");
        match scanner.poll() {
            Some(Segment::Fragment(frag)) => {
                assert_eq!(frag.sequence, Some(1));
                assert_eq!(&frag.bytes[frag.bytes.len() - 40..], &[7; 40]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_box_past_the_buffer_bound_is_named_rather_than_held() {
        let limits = Limits {
            max_buffered_box: 32,
            ..Limits::default()
        };
        let mut scanner = Scanner::with_limits(limits);
        let mut bytes = 1024u32.to_be_bytes().to_vec();
        bytes.extend_from_slice(b"moov");
        let err = scanner.push(&bytes).expect_err("a refusal");
        assert!(matches!(err, Error::Unsupported(_)), "{err}");
        assert!(err.to_string().contains("hold in memory"), "{err}");
    }

    #[test]
    fn a_stream_that_ends_inside_a_fragment_says_so() {
        let bytes = stream();
        let mut scanner = Scanner::new();
        scanner
            .push(&bytes[..bytes.len() - 20])
            .expect("a partial push");
        let err = scanner.finish().expect_err("a truncation");
        assert!(matches!(err, Error::Truncated(_)), "{err}");
    }

    #[test]
    fn a_moof_before_a_moov_is_refused() {
        let mut scanner = Scanner::new();
        let mut bytes = boxed(b"ftyp", b"iso5");
        bytes.extend_from_slice(&moof_bytes(1, true));
        let err = scanner.push(&bytes).expect_err("a refusal");
        assert_eq!(err.offset(), Some(12));
    }

    #[test]
    fn every_truncation_of_a_stream_is_an_error_and_not_a_panic() {
        let bytes = stream();
        for cut in 0..bytes.len() {
            let mut scanner = Scanner::new();
            let _ = scanner.push(&bytes[..cut]);
            while scanner.poll().is_some() {}
            let _ = scanner.finish();
        }
    }
}

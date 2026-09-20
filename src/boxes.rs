//! The box, and the cursor that walks what is inside one.
//!
//! ISO/IEC 14496-12 §4.2 gives a box three ways to say how long it is,
//! and all three are read here: a 32-bit `size`, a `size` of 1 followed
//! by a 64-bit `largesize`, and a `size` of 0 meaning the box runs to
//! the end of its parent, which at the top level is the end of the
//! file. The third one is the one implementations quietly drop; ffmpeg
//! writes it for the `mdat` of a stream it is muxing into a pipe, so a
//! reader that calls it malformed is a reader that cannot read that
//! file.
//!
//! **A declared size never becomes an allocation.** [`parse_header`]
//! returns the size; it does not go and get the bytes. What a caller
//! does next depends on which of the three faces of this crate it is:
//! the file reader turns a size into a span and reads the span through
//! [`Source`], which clamps it against the
//! file's real length; the streaming scanner waits for the bytes to
//! arrive and bounds what it will hold with
//! [`Limits::max_buffered_box`]; the in-memory walk in [`children`]
//! only ever narrows a slice it already has. That is why there is no
//! blanket "no box may be larger than N" rule: the rule would have to
//! be wrong for `mdat`, and it is not the rule that keeps a reader
//! safe.
//!
//! [`Source`]: crate::source::Source

use std::io::{Read, Seek};

use crate::source::Source;
use crate::{Error, Limits, Result, MAX_CHILDREN};

/// How long a box says it is (§4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoxSize {
    /// A `size` or `largesize` that names the total length of the box,
    /// its own header included.
    Exact(u64),
    /// A `size` of 0: the box runs to the end of its parent, which at
    /// the top level is the end of the file, and in a stream is the
    /// end of the stream.
    ToEnd,
}

/// A box header as its own bytes describe it, resolved against nothing
/// (§4.2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoxHeader {
    /// The four characters of `boxtype`.
    pub kind: [u8; 4],
    /// How long the header itself is: 8, or 16 when a `largesize`
    /// follows.
    pub header_len: u64,
    /// What `size` said.
    pub size: BoxSize,
}

impl BoxHeader {
    pub fn is(&self, kind: &[u8; 4]) -> bool {
        &self.kind == kind
    }

    /// The total length of the box, given where its parent ends.
    pub fn total(&self, from: u64, limit: u64) -> Result<u64> {
        match self.size {
            BoxSize::Exact(size) => Ok(size),
            BoxSize::ToEnd => limit
                .checked_sub(from)
                .ok_or_else(|| Error::format("a box that begins past its own parent").at(from)),
        }
    }
}

/// The header at the head of `data`, or `None` when `data` is shorter
/// than the header itself, which in a stream means "not yet" and in a
/// file means "no more boxes".
///
/// `at` is the absolute position of `data[0]`, carried only so an error
/// can say where it was.
pub fn parse_header(data: &[u8], at: u64) -> Result<Option<BoxHeader>> {
    if data.len() < 8 {
        return Ok(None);
    }
    let short = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let mut kind = [0u8; 4];
    kind.copy_from_slice(&data[4..8]);
    let (size, header_len) = match short {
        1 => {
            if data.len() < 16 {
                return Ok(None);
            }
            let mut wide = [0u8; 8];
            wide.copy_from_slice(&data[8..16]);
            (BoxSize::Exact(u64::from_be_bytes(wide)), 16u64)
        }
        0 => (BoxSize::ToEnd, 8u64),
        other => (BoxSize::Exact(u64::from(other)), 8u64),
    };
    if let BoxSize::Exact(size) = size {
        if size < header_len {
            return Err(Error::format("a box shorter than its own header")
                .at(at)
                .in_box(kind));
        }
    }
    Ok(Some(BoxHeader {
        kind,
        header_len,
        size,
    }))
}

/// One box of a file, placed: where it begins, where its body begins,
/// and where it ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BoxSpan {
    pub kind: [u8; 4],
    /// Where the box begins, at its `size` field.
    pub start: u64,
    /// Where its body begins, past `size`, `boxtype` and any
    /// `largesize`.
    pub body: u64,
    /// Where it ends.
    pub end: u64,
}

impl BoxSpan {
    pub fn is(&self, kind: &[u8; 4]) -> bool {
        &self.kind == kind
    }

    pub fn body_len(&self) -> u64 {
        self.end.saturating_sub(self.body)
    }
}

/// The header of the box at `at`, read through the source, or `None`
/// when `limit` leaves no room for one.
///
/// A `size` of 0 resolves to `limit`, which is what §4.2 means by "to
/// the end of the file" when the parent is the file itself.
pub fn read_header<R: Read + Seek>(
    src: &mut Source<R>,
    at: u64,
    limit: u64,
) -> Result<Option<BoxSpan>> {
    if limit < at || limit - at < 8 {
        return Ok(None);
    }
    let want = usize::try_from((limit - at).min(16)).unwrap_or(16);
    let head = src.read_at(at, want)?;
    let Some(header) = parse_header(&head, at)? else {
        // Eight bytes were there, so this is a `largesize` whose own
        // bytes the file does not reach: a truncation, not an end.
        return Err(Error::truncated("a box that ends inside its own size").at(at));
    };
    let size = header.total(at, limit)?;
    let body = at + header.header_len;
    let end = at
        .checked_add(size)
        .ok_or_else(|| Error::format("a box whose size overflows the file").at(at))?;
    if end > limit || body > end {
        return Err(Error::format("a box that runs past the file or its parent")
            .at(at)
            .in_box(header.kind));
    }
    Ok(Some(BoxSpan {
        kind: header.kind,
        start: at,
        body,
        end,
    }))
}

/// Every top-level box of a file, in order (§4.2).
pub fn top_level<R: Read + Seek>(src: &mut Source<R>) -> Result<Vec<BoxSpan>> {
    let limit = src.len();
    let max = src.limits().max_top_level;
    let mut out = Vec::new();
    let mut at = 0u64;
    while let Some(header) = read_header(src, at, limit)? {
        if header.end <= at {
            return Err(Error::format("a box of no length, which would not end").at(at));
        }
        at = header.end;
        out.push(header);
        if out.len() > max {
            return Err(Error::format("more top-level boxes than a file has"));
        }
    }
    Ok(out)
}

/// One child box of a body already in hand.
#[derive(Clone, Copy, Debug)]
pub struct Child<'a> {
    pub kind: [u8; 4],
    /// The child's body, which is a slice of the parent's.
    pub body: &'a [u8],
    /// Where `body[0]` sits in the file or stream, when the caller
    /// knew where the parent was.
    pub at: u64,
}

impl Child<'_> {
    pub fn is(&self, kind: &[u8; 4]) -> bool {
        &self.kind == kind
    }
}

/// The child boxes of a body, flat, with the default child bound.
///
/// `at` is where `body[0]` sits in the file, or 0 when the caller does
/// not know or does not care.
pub fn children(body: &[u8], at: u64) -> Result<Vec<Child<'_>>> {
    children_bounded(body, at, MAX_CHILDREN)
}

/// The same, bounded by a caller's own [`Limits::max_children`].
pub fn children_bounded(body: &[u8], at: u64, max: usize) -> Result<Vec<Child<'_>>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos + 8 <= body.len() {
        let here = at + pos as u64;
        let header = parse_header(&body[pos..], here)?
            .ok_or_else(|| Error::truncated("a box that ends inside its own size").at(here))?;
        // Inside a body the parent's own end is the limit, so a size of
        // 0 is the rest of the parent.
        let size = match header.size {
            BoxSize::Exact(size) => usize::try_from(size)
                .map_err(|_| Error::format("a box wider than memory").at(here))?,
            BoxSize::ToEnd => body.len() - pos,
        };
        let head = usize::try_from(header.header_len).unwrap_or(usize::MAX);
        if size < head || size > body.len() - pos {
            return Err(Error::format("a child box that runs past its parent")
                .at(here)
                .in_box(header.kind));
        }
        out.push(Child {
            kind: header.kind,
            body: &body[pos + head..pos + size],
            at: here + header.header_len,
        });
        pos += size;
        if out.len() > max {
            return Err(Error::format("more child boxes than a box has").at(at));
        }
    }
    Ok(out)
}

/// The body of the first child of a kind.
pub fn child<'a>(list: &[Child<'a>], kind: &[u8; 4]) -> Option<&'a [u8]> {
    list.iter().find(|item| item.is(kind)).map(|item| item.body)
}

/// The first child of a kind, as a child rather than a body, so its
/// position survives.
pub fn child_of<'a>(list: &[Child<'a>], kind: &[u8; 4]) -> Option<Child<'a>> {
    list.iter().find(|item| item.is(kind)).copied()
}

/// The body of the first top-level box of a kind in a segment already
/// in memory, which is what an init segment is.
pub fn find_in<'a>(data: &'a [u8], kind: &[u8; 4], at: u64) -> Result<Option<&'a [u8]>> {
    Ok(child(&children(data, at)?, kind))
}

// ------------------------------------------------------------------ //
// The cursor.
// ------------------------------------------------------------------ //

/// A bounds-checked walk over bytes already in hand, big-endian, which
/// is the byte order every field in §4 and §8 is written in.
///
/// It carries the absolute position of its first byte so that an error
/// from twelve fields into a `trun` says where the `trun` was.
#[derive(Clone, Copy, Debug)]
pub struct Bytes<'a> {
    data: &'a [u8],
    at: usize,
    base: u64,
}

impl<'a> Bytes<'a> {
    /// A cursor over bytes whose file position is not known.
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            at: 0,
            base: 0,
        }
    }

    /// A cursor over bytes that begin at `base` in the file.
    pub fn at(data: &'a [u8], base: u64) -> Self {
        Self { data, at: 0, base }
    }

    /// Where the cursor is, in the file.
    pub fn offset(&self) -> u64 {
        self.base + self.at as u64
    }

    pub fn left(&self) -> usize {
        self.data.len().saturating_sub(self.at)
    }

    pub fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(count)
            .ok_or_else(|| Error::format("a length that overflows").at(self.offset()))?;
        let out = self
            .data
            .get(self.at..end)
            .ok_or_else(|| Error::truncated("the bytes end inside a value").at(self.offset()))?;
        self.at = end;
        Ok(out)
    }

    pub fn rest(&mut self) -> &'a [u8] {
        let out = &self.data[self.at.min(self.data.len())..];
        self.at = self.data.len();
        out
    }

    pub fn skip(&mut self, count: usize) -> Result<()> {
        self.take(count).map(|_| ())
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    pub fn u32(&mut self) -> Result<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// The same four bytes, read as signed. §8.6.1.3 and §8.8.8 both
    /// have a version 0 field that is unsigned on paper and signed in
    /// every file ffmpeg wrote; see [`crate::fragment`].
    pub fn i32(&mut self) -> Result<i32> {
        self.u32().map(|value| value as i32)
    }

    pub fn u64(&mut self) -> Result<u64> {
        let bytes = self.take(8)?;
        let mut out = [0u8; 8];
        out.copy_from_slice(bytes);
        Ok(u64::from_be_bytes(out))
    }

    /// A `FullBox`'s version byte and its three flag bytes, in one
    /// (§4.2).
    pub fn full_box(&mut self) -> Result<(u8, u32)> {
        let version = self.u8()?;
        let bytes = self.take(3)?;
        Ok((
            version,
            u32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]]),
        ))
    }

    /// An `entry_count`, refused when the entries it claims could not
    /// fit in the bytes that are left.
    ///
    /// This is the whole of "never allocate from an unchecked length"
    /// for the sample tables: a header saying four billion entries is a
    /// lie the box's own length contradicts, and catching it here is
    /// what keeps a `Vec` from being told to hold four billion of
    /// anything.
    pub fn count(&mut self, entry_size: usize) -> Result<usize> {
        let at = self.offset();
        let count = self.u32()? as usize;
        let want = count
            .checked_mul(entry_size.max(1))
            .ok_or_else(|| Error::format("a table wider than memory").at(at))?;
        if want > self.left() {
            return Err(Error::format("a table longer than the box holding it").at(at));
        }
        Ok(count)
    }
}

// ------------------------------------------------------------------ //
// Building one.
// ------------------------------------------------------------------ //

/// A box: its length, its four characters, and its payload (§4.2).
pub fn boxed(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + payload.len());
    out.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    out
}

/// A `FullBox`: a box whose payload opens with a version byte and three
/// flag bytes (§4.2).
pub fn full_boxed(kind: &[u8; 4], version: u8, flags: u32, payload: &[u8]) -> Vec<u8> {
    let mut full = Vec::with_capacity(4 + payload.len());
    full.push(version);
    full.extend_from_slice(&flags.to_be_bytes()[1..]);
    full.extend_from_slice(payload);
    boxed(kind, &full)
}

/// A `uuid` box: a box whose type is an extended 16-byte type, which is
/// how §4.2 lets anyone carry anything in a file without asking (§4.2,
/// `extended_type`).
pub fn uuid_boxed(uuid: &[u8; 16], payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::with_capacity(16 + payload.len());
    body.extend_from_slice(uuid);
    body.extend_from_slice(payload);
    boxed(b"uuid", &body)
}

/// The child bound a [`Limits`] asks for, for the walks inside this
/// crate.
pub(crate) fn kids<'a>(body: &'a [u8], at: u64, limits: &Limits) -> Result<Vec<Child<'a>>> {
    children_bounded(body, at, limits.max_children)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn source(bytes: Vec<u8>) -> Source<Cursor<Vec<u8>>> {
        Source::new(Cursor::new(bytes)).expect("a source")
    }

    #[test]
    fn a_box_of_every_size_shape_reads() {
        let mut file = boxed(b"ftyp", b"isom");
        let mut wide = 1u32.to_be_bytes().to_vec();
        wide.extend_from_slice(b"free");
        wide.extend_from_slice(&20u64.to_be_bytes());
        wide.extend_from_slice(&[0; 4]);
        file.extend_from_slice(&wide);
        // A size of zero runs to the end of the file.
        file.extend_from_slice(&[0, 0, 0, 0]);
        file.extend_from_slice(b"mdat");
        file.extend_from_slice(&[9; 16]);
        let mut src = source(file);
        let top = top_level(&mut src).expect("the boxes");
        assert_eq!(top.len(), 3);
        assert_eq!(top[1].body_len(), 4, "a largesize box's body");
        assert_eq!(top[2].end, src.len(), "a size of zero runs to the end");
    }

    #[test]
    fn a_size_of_zero_inside_a_body_runs_to_the_end_of_the_parent() {
        let mut body = boxed(b"mfhd", &[0; 8]);
        body.extend_from_slice(&[0, 0, 0, 0]);
        body.extend_from_slice(b"traf");
        body.extend_from_slice(&[1; 12]);
        let kids = children(&body, 0).expect("the children");
        assert_eq!(kids.len(), 2);
        assert!(kids[1].is(b"traf"));
        assert_eq!(kids[1].body.len(), 12);
    }

    #[test]
    fn a_box_that_runs_past_the_file_is_refused() {
        let mut file = boxed(b"ftyp", b"isom");
        file.extend_from_slice(&1_000_000u32.to_be_bytes());
        file.extend_from_slice(b"moov");
        let mut src = source(file);
        let err = top_level(&mut src).expect_err("a refusal");
        assert_eq!(err.offset(), Some(12), "the error says where");
    }

    #[test]
    fn a_box_smaller_than_its_own_header_is_refused() {
        // A size of 4 with an eight-byte header, and a largesize of 8
        // with a sixteen-byte one.
        assert!(parse_header(&[0, 0, 0, 4, b'f', b'r', b'e', b'e'], 0).is_err());
        let mut wide = 1u32.to_be_bytes().to_vec();
        wide.extend_from_slice(b"free");
        wide.extend_from_slice(&8u64.to_be_bytes());
        assert!(parse_header(&wide, 0).is_err());
        // Fewer bytes than the header needs is "not yet", not an error.
        assert_eq!(parse_header(&[0, 0, 0, 16], 0).expect("no header"), None);
        assert_eq!(parse_header(&wide[..12], 0).expect("no header"), None);
    }

    #[test]
    fn a_box_past_a_quarter_gigabyte_is_read_rather_than_refused() {
        // The old streaming copy capped every box at 1<<28. An mdat of
        // a long film is larger than that and is not malformed, so the
        // header layer states the size and leaves the bounding to
        // whoever has to hold the bytes.
        let mut head = (1u32 << 29).to_be_bytes().to_vec();
        head.extend_from_slice(b"mdat");
        let header = parse_header(&head, 0).expect("a header").expect("a header");
        assert_eq!(header.size, BoxSize::Exact(1 << 29));
    }

    #[test]
    fn a_cursor_never_reads_past_what_it_was_given() {
        let mut bytes = Bytes::new(&[0, 0, 0, 2, 9, 9]);
        assert_eq!(bytes.count(1).expect("a count"), 2);
        assert_eq!(bytes.take(2).expect("the entries"), &[9, 9]);
        assert!(bytes.u8().is_err());

        let mut wide = Bytes::new(&[0xff, 0xff, 0xff, 0xff]);
        assert!(wide.count(8).is_err(), "a count the box cannot hold");

        let mut short = Bytes::new(&[1, 2]);
        assert!(short.u32().is_err());
        assert!(short.take(usize::MAX).is_err());
    }

    #[test]
    fn a_cursor_says_where_in_the_file_it_gave_up() {
        let mut bytes = Bytes::at(&[0, 0, 0, 1, 7], 1000);
        assert_eq!(bytes.u32().expect("a word"), 1);
        assert_eq!(bytes.offset(), 1004);
        let err = bytes.u32().expect_err("a truncation");
        assert_eq!(err.offset(), Some(1004));
    }

    #[test]
    fn the_builders_write_what_the_parsers_read() {
        let plain = boxed(b"free", &[1, 2, 3]);
        assert_eq!(plain.len(), 11);
        let full = full_boxed(b"tfdt", 1, 0, &7u64.to_be_bytes());
        let kids = children(&full, 0).expect("a child");
        let mut cursor = Bytes::new(kids[0].body);
        assert_eq!(cursor.full_box().expect("version"), (1, 0));
        assert_eq!(cursor.u64().expect("the time"), 7);
        let uuid = uuid_boxed(&[0xab; 16], b"payload");
        assert_eq!(&uuid[4..8], b"uuid");
        assert_eq!(&uuid[8..24], &[0xab; 16]);
        assert_eq!(&uuid[24..], b"payload");
    }
}

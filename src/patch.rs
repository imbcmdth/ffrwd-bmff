//! Finding, appending and replacing a top-level box, in place.
//!
//! §4.2 lets anyone put anything in an ISO base media file: a box of a
//! type a player does not know is skipped, and a `uuid` box with an
//! extended type nobody else has registered cannot collide with
//! anything. Putting such a box at the end of a file moves nothing
//! else, so writing one costs one append whatever the file's size, and
//! the `moov` and `mdat` offsets every sample table holds stay true.
//!
//! What goes in the box is the caller's business. This module takes a
//! [`Selector`] and a payload and knows nothing about either.
//!
//! Two files need a word more.
//!
//! **A file that already has one.** When the box is the last thing in
//! the file it is cut off and written again, which is the same append.
//! When it is not, the file has to be copied without it, and
//! [`install`] refuses to do that unless [`rewrite`] is asked for,
//! because a copy is not what "in place" promised.
//!
//! **A fragmented file.** The box goes before an `mfra` (§8.8.9), and
//! the reason is `mfro` (§8.8.11): it is a copy of `mfra`'s own size,
//! placed last so that the last four bytes of the file find it.
//! Appending past the `mfra` leaves those four bytes pointing at
//! something else, and a reader that uses them loses its random
//! access; ffmpeg does not, because it builds the fragment index by
//! walking the file, but a reader that does is not wrong to. So the
//! box goes in front of the `mfra` and the `mfra` is written again
//! after it. That costs the `mfra`'s own length, which is sixteen
//! bytes a fragment, and moves nothing that anything points at: the
//! offsets inside `tfra` (§8.8.10) name the `moof` boxes, and they are
//! all before the `mfra` already.

use std::io::{Read, Seek, SeekFrom, Write};

use crate::boxes;
use crate::source::Source;
use crate::{Error, Result};

/// Which top-level box a call is about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selector {
    /// A box by its four characters.
    Kind([u8; 4]),
    /// A `uuid` box by its 16-byte extended type (§4.2).
    Uuid([u8; 16]),
}

impl Selector {
    /// The four characters the box's own header carries.
    pub fn kind(&self) -> [u8; 4] {
        match self {
            Selector::Kind(kind) => *kind,
            Selector::Uuid(_) => *b"uuid",
        }
    }

    /// How many bytes of the body belong to the type rather than to
    /// the payload: 16 for a `uuid` box, none otherwise.
    pub fn prefix(&self) -> u64 {
        match self {
            Selector::Kind(_) => 0,
            Selector::Uuid(_) => 16,
        }
    }

    /// A box of this selector around a payload.
    pub fn boxed(&self, payload: &[u8]) -> Vec<u8> {
        match self {
            Selector::Kind(kind) => boxes::boxed(kind, payload),
            Selector::Uuid(uuid) => boxes::uuid_boxed(uuid, payload),
        }
    }

    /// Whether a body that begins with these bytes is this selector's.
    fn claims(&self, head: &[u8]) -> bool {
        match self {
            Selector::Kind(_) => true,
            Selector::Uuid(uuid) => head.len() >= 16 && head[..16] == uuid[..],
        }
    }
}

/// A box of the selector's already in a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Found {
    /// Where the box begins.
    pub start: u64,
    /// Where the payload inside it begins, past the header and, for a
    /// `uuid` box, past the extended type.
    pub body: u64,
    /// Where the box ends.
    pub end: u64,
    /// Whether it is the last top-level box of the file.
    pub last: bool,
}

/// How far back from the end of a file the tail guess looks for the
/// header of a box that ends there.
const TAIL_WINDOW: usize = 4096;

/// This selector's box, if the file has one.
///
/// The tail first, because the end of the file is where [`install`]
/// puts it and that is where it will be for every file this crate
/// wrote: one read of the last few hundred bytes answers the question.
/// Only when that misses are the top-level boxes walked.
pub fn find<R: Read + Seek>(src: &mut Source<R>, selector: Selector) -> Result<Option<Found>> {
    if let Some(found) = at_tail(src, selector)? {
        return Ok(Some(found));
    }
    let top = boxes::top_level(src)?;
    let prefix = selector.prefix();
    for (index, header) in top.iter().enumerate() {
        if !header.is(&selector.kind()) || header.body_len() < prefix {
            continue;
        }
        let head = src.read_at(header.body, prefix as usize)?;
        if !selector.claims(&head) {
            continue;
        }
        return Ok(Some(Found {
            start: header.start,
            body: header.body + prefix,
            end: header.end,
            last: index + 1 == top.len(),
        }));
    }
    Ok(None)
}

/// The tail guess: a box of the selector's whose end is the end of the
/// file.
///
/// The box's size field is at its own start, which is not known, so
/// this reads one window at the end and takes the earliest header in
/// it whose size lands exactly on the end of the file and whose type
/// is the one wanted. Wrong guesses cost nothing: the size has to hit
/// the end of the file to the byte, and for a `uuid` box the sixteen
/// bytes after the header have to be the right ones.
fn at_tail<R: Read + Seek>(src: &mut Source<R>, selector: Selector) -> Result<Option<Found>> {
    let len = src.len();
    let prefix = selector.prefix() as usize;
    let least = 8 + prefix;
    if len < least as u64 {
        return Ok(None);
    }
    let window = src.read_at(len.saturating_sub(TAIL_WINDOW as u64), TAIL_WINDOW)?;
    let base = len - window.len() as u64;
    let kind = selector.kind();
    for at in 0..window.len().saturating_sub(least) {
        if window[at + 4..at + 8] != kind {
            continue;
        }
        let short =
            u32::from_be_bytes([window[at], window[at + 1], window[at + 2], window[at + 3]]);
        let (size, head) = if short == 1 {
            let Some(wide) = window.get(at + 8..at + 16) else {
                continue;
            };
            let mut value = [0u8; 8];
            value.copy_from_slice(wide);
            (u64::from_be_bytes(value), 16usize)
        } else {
            (u64::from(short), 8usize)
        };
        if size < (head + prefix) as u64 || base + at as u64 + size != len {
            continue;
        }
        if !selector.claims(window.get(at + head..).unwrap_or_default()) {
            continue;
        }
        let start = base + at as u64;
        return Ok(Some(Found {
            start,
            body: start + (head + prefix) as u64,
            end: len,
            last: true,
        }));
    }
    Ok(None)
}

/// The payload of this selector's box, if the file carries one.
pub fn read<R: Read + Seek>(src: &mut Source<R>, selector: Selector) -> Result<Option<Vec<u8>>> {
    let Some(found) = find(src, selector)? else {
        return Ok(None);
    };
    Ok(Some(src.span(found.body, found.end - found.body)?))
}

/// Where a new box goes, and what it costs to put it there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spot {
    /// The end of the file. Nothing else moves.
    End(u64),
    /// Just before the file's `mfra`, which has to stay last; see this
    /// module's head.
    BeforeMfra(u64),
    /// Over the box already there, which is last so it can be cut off.
    Over(u64),
    /// A box of the selector's is in the file but not at the end, so
    /// the file has to be copied without it. The span is the box to
    /// leave out.
    Rewrite(u64, u64),
}

/// Where this file's box of the selector belongs.
pub fn spot<R: Read + Seek>(src: &mut Source<R>, selector: Selector) -> Result<Spot> {
    let top = boxes::top_level(src)?;
    if let Some(found) = find(src, selector)? {
        return Ok(if found.last {
            Spot::Over(found.start)
        } else {
            Spot::Rewrite(found.start, found.end)
        });
    }
    match top.last() {
        Some(last) if last.is(b"mfra") => Ok(Spot::BeforeMfra(last.start)),
        _ => Ok(Spot::End(src.len())),
    }
}

/// Cutting a file short, which neither [`Write`] nor [`Seek`] can do.
pub trait Truncate {
    fn truncate_to(&mut self, len: u64) -> std::io::Result<()>;
}

impl Truncate for std::fs::File {
    fn truncate_to(&mut self, len: u64) -> std::io::Result<()> {
        self.set_len(len)
    }
}

impl Truncate for std::io::Cursor<Vec<u8>> {
    fn truncate_to(&mut self, len: u64) -> std::io::Result<()> {
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        self.get_mut().truncate(len);
        Ok(())
    }
}

impl<T: Truncate + ?Sized> Truncate for &mut T {
    fn truncate_to(&mut self, len: u64) -> std::io::Result<()> {
        (**self).truncate_to(len)
    }
}

/// What writing the box did to the file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placed {
    /// Appended, with nothing else touched.
    Appended { at: u64 },
    /// Appended over the box that was already last.
    Replaced { at: u64 },
    /// Put in front of the file's `mfra`, which was written again
    /// after it.
    BeforeMfra { at: u64, moved: u64 },
}

/// Writes the box into a file that is open for reading and writing.
///
/// The file is read through a [`Source`] first, so the cost of finding
/// where the box goes is counted like any other read.
pub fn install<T: Read + Write + Seek + Truncate>(
    file: T,
    selector: Selector,
    payload: &[u8],
) -> Result<Placed> {
    let mut src = Source::new(file)?;
    let ceiling = src.limits().max_read as u64;
    let where_it_goes = spot(&mut src, selector)?;
    let built = selector.boxed(payload);
    match where_it_goes {
        Spot::End(at) => {
            let file = src.inner_mut();
            file.seek(SeekFrom::Start(at))?;
            file.write_all(&built)?;
            file.flush()?;
            Ok(Placed::Appended { at })
        }
        Spot::Over(at) => {
            let file = src.inner_mut();
            file.truncate_to(at)?;
            file.seek(SeekFrom::Start(at))?;
            file.write_all(&built)?;
            file.flush()?;
            Ok(Placed::Replaced { at })
        }
        Spot::BeforeMfra(at) => {
            let moved = src.len() - at;
            if moved > ceiling {
                return Err(Error::unsupported(
                    "an mfra larger than this writer will hold to move it",
                )
                .at(at));
            }
            let mfra = src.span(at, moved)?;
            let file = src.inner_mut();
            file.truncate_to(at)?;
            file.seek(SeekFrom::Start(at))?;
            file.write_all(&built)?;
            file.write_all(&mfra)?;
            file.flush()?;
            Ok(Placed::BeforeMfra { at, moved })
        }
        Spot::Rewrite(start, _) => Err(Error::unsupported(
            "the file already carries a box of this type, and it is not the last box, so it \
             cannot be replaced without copying the file",
        )
        .at(start)),
    }
}

/// Copies a file without the box it already has, then appends the new
/// one.
pub fn rewrite<R: Read + Seek, W: Write>(
    src: &mut Source<R>,
    out: &mut W,
    selector: Selector,
    payload: &[u8],
) -> Result<()> {
    let (start, end) = match spot(src, selector)? {
        Spot::Rewrite(start, end) => (start, end),
        Spot::Over(start) => (start, src.len()),
        _ => (src.len(), src.len()),
    };
    copy_span(src, out, 0, start)?;
    copy_span(src, out, end, src.len())?;
    out.write_all(&selector.boxed(payload))?;
    out.flush()?;
    Ok(())
}

/// How much of a file is copied at a time.
const CHUNK: usize = 1 << 20;

fn copy_span<R: Read + Seek, W: Write>(
    src: &mut Source<R>,
    out: &mut W,
    from: u64,
    to: u64,
) -> Result<()> {
    let mut at = from;
    while at < to {
        let want = usize::try_from(to - at).unwrap_or(CHUNK).min(CHUNK);
        let bytes = src.read_at(at, want)?;
        if bytes.is_empty() {
            break;
        }
        out.write_all(&bytes)?;
        at += bytes.len() as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxes::boxed;
    use std::io::Cursor;

    const UUID: [u8; 16] = [
        0x8f, 0x2c, 0x4b, 0x1d, 0x7a, 0x64, 0x41, 0xe9, 0xb3, 0x05, 0x26, 0x7c, 0x0a, 0x5d, 0x31,
        0x88,
    ];

    fn selector() -> Selector {
        Selector::Uuid(UUID)
    }

    fn plain() -> Vec<u8> {
        let mut file = boxed(b"ftyp", b"isom");
        file.extend_from_slice(&boxed(b"mdat", &[5; 64]));
        file.extend_from_slice(&boxed(b"moov", &[0; 32]));
        file
    }

    fn payload(byte: u8) -> Vec<u8> {
        vec![b'F', b'F', b'I', b'X', 1, byte]
    }

    fn source(bytes: Vec<u8>) -> Source<Cursor<Vec<u8>>> {
        Source::new(Cursor::new(bytes)).expect("a source")
    }

    #[test]
    fn a_box_is_appended_and_then_written_over() {
        let before = plain();
        let mut file = Cursor::new(before.clone());
        let placed = install(&mut file, selector(), &payload(0)).expect("a write");
        assert_eq!(
            placed,
            Placed::Appended {
                at: before.len() as u64
            }
        );
        assert_eq!(
            &file.get_ref()[..before.len()],
            &before[..],
            "nothing moved"
        );

        // A second write lands on top of the first, so the file does
        // not grow a box a read.
        let grown = file.get_ref().len();
        let placed = install(&mut file, selector(), &payload(1)).expect("a write");
        assert_eq!(
            placed,
            Placed::Replaced {
                at: before.len() as u64
            }
        );
        assert_eq!(file.get_ref().len(), grown);
        let mut src = source(file.into_inner());
        assert_eq!(
            read(&mut src, selector()).expect("a read").expect("a box"),
            payload(1)
        );
        assert!(src.tally().bytes_read < 8192, "the tail guess found it");
    }

    #[test]
    fn a_fragmented_file_keeps_its_mfra_last() {
        let mut before = boxed(b"ftyp", b"isom");
        before.extend_from_slice(&boxed(b"moof", &[0; 16]));
        before.extend_from_slice(&boxed(b"mdat", &[5; 64]));
        let at = before.len() as u64;
        let mfra = boxed(b"mfra", &[1, 2, 3, 4]);
        before.extend_from_slice(&mfra);

        let mut file = Cursor::new(before.clone());
        let placed = install(&mut file, selector(), &payload(2)).expect("a write");
        assert_eq!(
            placed,
            Placed::BeforeMfra {
                at,
                moved: mfra.len() as u64
            }
        );
        let after = file.into_inner();
        assert_eq!(
            &after[after.len() - mfra.len()..],
            &mfra[..],
            "mfra is last"
        );
        let mut src = source(after);
        assert_eq!(
            read(&mut src, selector()).expect("a read").expect("a box"),
            payload(2)
        );
    }

    #[test]
    fn a_box_in_the_middle_is_refused_until_a_rewrite_is_asked_for() {
        let mut before = boxed(b"ftyp", b"isom");
        let at = before.len() as u64;
        before.extend_from_slice(&selector().boxed(&payload(3)));
        let end = before.len() as u64;
        before.extend_from_slice(&boxed(b"mdat", &[5; 64]));
        before.extend_from_slice(&boxed(b"moov", &[0; 32]));

        let mut src = source(before.clone());
        assert_eq!(
            spot(&mut src, selector()).expect("a spot"),
            Spot::Rewrite(at, end)
        );

        let mut file = Cursor::new(before.clone());
        let err = install(&mut file, selector(), &payload(4)).expect_err("a refusal");
        assert_eq!(err.offset(), Some(at));
        assert_eq!(file.into_inner(), before, "the file was left alone");

        let mut src = source(before);
        let mut out: Vec<u8> = Vec::new();
        rewrite(&mut src, &mut out, selector(), &payload(4)).expect("a rewrite");
        let mut src = source(out);
        assert_eq!(
            read(&mut src, selector()).expect("a read").expect("a box"),
            payload(4)
        );
        // And there is only one of them left.
        let top = boxes::top_level(&mut src).expect("the boxes");
        assert_eq!(top.iter().filter(|header| header.is(b"uuid")).count(), 1);
    }

    #[test]
    fn a_plain_four_character_box_works_the_same_way() {
        let selector = Selector::Kind(*b"free");
        let before = plain();
        let mut file = Cursor::new(before.clone());
        install(&mut file, selector, b"anything").expect("a write");
        let mut src = source(file.into_inner());
        assert_eq!(
            read(&mut src, selector).expect("a read").expect("a box"),
            b"anything".to_vec()
        );
    }

    #[test]
    fn a_uuid_box_of_someone_else_is_not_ours() {
        let mut file = plain();
        file.extend_from_slice(&crate::boxes::uuid_boxed(&[0x11; 16], b"theirs"));
        let mut src = source(file);
        assert_eq!(find(&mut src, selector()).expect("a walk"), None);
    }

    #[test]
    fn every_truncation_of_a_patched_file_is_an_error_and_not_a_panic() {
        let mut file = plain();
        file.extend_from_slice(&selector().boxed(&payload(9)));
        for cut in 0..file.len() {
            let mut src = source(file[..cut].to_vec());
            let _ = find(&mut src, selector());
            let _ = read(&mut src, selector());
            let _ = spot(&mut src, selector());
        }
    }
}

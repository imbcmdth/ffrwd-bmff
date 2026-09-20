//! Movie fragments: `moof`, `traf`, `tfhd`, `tfdt`, `trun` (§8.8).
//!
//! # Where a run's data is
//!
//! This is the clause implementations get wrong, so it is written out
//! once here and tested in three shapes. §8.8.7 gives `tfhd` two flags
//! and a default:
//!
//! 1. `base-data-offset-present` (0x000001): the `base_data_offset`
//!    field says where, as an offset in the file.
//! 2. otherwise `default-base-is-moof` (0x020000): the base is the
//!    first byte of the enclosing `moof` box.
//! 3. otherwise: for **the first** track fragment of the movie
//!    fragment, the base is the first byte of the enclosing `moof`;
//!    for **each one after it**, the base is the end of the data the
//!    preceding track fragment described.
//!
//! Two copies of this code used to exist and neither had case 3. One
//! took the `moof` start for every track fragment, which puts the
//! second track's samples on top of the first's in any file written
//! without either flag; the other refused a `moof` with more than one
//! `traf` outright. Both are right about the single-track fragment a
//! live stream carries, and both are wrong about a muxed file.
//!
//! §8.8.8 then chains the runs inside one track fragment: a `trun`
//! with `data-offset-present` starts at `base + data_offset`, and one
//! without starts where the previous run's data ended, or at the base
//! when it is the first.
//!
//! # Composition offsets
//!
//! §8.8.8's `sample_composition_time_offset` is unsigned in a version
//! 0 `trun` and signed in a version 1 one. ffmpeg writes negative
//! offsets into version 0 boxes, and a reader that believes the
//! standard there reports a sample two billion ticks in the future.
//! Both versions are read signed, which is what ffmpeg's own reader
//! does and what [`crate::track`] does with the matching `ctts` field.
//! A file that genuinely meant an offset above 2^31 ticks would be
//! misread, and no muxer writes one: at a 90 kHz timescale that is
//! thirteen hours of reordering.

use crate::boxes::{self, Bytes, Child};
use crate::track::{Sample, Track, TrexDefaults};
use crate::{Error, Limits, Result};

/// The `sample_is_non_sync_sample` bit of §8.8.3's sample flags: clear
/// on a sync sample.
pub const NON_SYNC: u32 = 0x0001_0000;

/// What a `tfhd` settles for its track fragment (§8.8.7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tfhd {
    pub track_id: u32,
    /// The explicit `base_data_offset`, when the flag said there is
    /// one.
    pub base_data_offset: Option<u64>,
    /// Whether `default-base-is-moof` was set.
    pub base_is_moof: bool,
    pub default_duration: Option<u32>,
    pub default_size: Option<u32>,
    pub default_flags: Option<u32>,
}

impl Tfhd {
    /// Where this track fragment's data begins, given where the `moof`
    /// begins, whether this is the movie fragment's first track
    /// fragment, and where the previous one's data ended.
    ///
    /// The three cases of §8.8.7, in the order the clause states them.
    pub fn base(&self, moof_start: u64, first: bool, previous_end: u64) -> u64 {
        match self.base_data_offset {
            Some(explicit) => explicit,
            None if self.base_is_moof => moof_start,
            None if first => moof_start,
            None => previous_end,
        }
    }
}

/// One `trun` entry, as the flags said to read it (§8.8.8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub duration: u32,
    pub size: u32,
    pub flags: u32,
    pub composition_offset: i32,
}

/// One `tfhd` read (§8.8.7).
pub fn read_tfhd(body: &[u8], at: u64) -> Result<Tfhd> {
    let mut bytes = Bytes::at(body, at);
    let (_, flags) = bytes.full_box()?;
    let track_id = bytes.u32()?;
    let base_data_offset = if flags & 0x01 != 0 {
        Some(bytes.u64()?)
    } else {
        None
    };
    if flags & 0x02 != 0 {
        bytes.skip(4)?; // sample_description_index
    }
    let default_duration = if flags & 0x08 != 0 {
        Some(bytes.u32()?)
    } else {
        None
    };
    let default_size = if flags & 0x10 != 0 {
        Some(bytes.u32()?)
    } else {
        None
    };
    let default_flags = if flags & 0x20 != 0 {
        Some(bytes.u32()?)
    } else {
        None
    };
    Ok(Tfhd {
        track_id,
        base_data_offset,
        base_is_moof: flags & 0x02_0000 != 0,
        default_duration,
        default_size,
        default_flags,
    })
}

/// A `tfdt`'s `baseMediaDecodeTime`, either width (§8.8.12).
pub fn read_tfdt(body: &[u8], at: u64) -> Result<i64> {
    let mut bytes = Bytes::at(body, at);
    let (version, _) = bytes.full_box()?;
    Ok(if version == 1 {
        bytes.u64()? as i64
    } else {
        i64::from(bytes.u32()?)
    })
}

/// A `trun`'s entries and where its data starts, relative to the track
/// fragment's base (§8.8.8).
pub fn read_trun(
    body: &[u8],
    at: u64,
    tfhd: &Tfhd,
    trex: &TrexDefaults,
    limits: &Limits,
) -> Result<(Option<i64>, Vec<Entry>)> {
    let mut bytes = Bytes::at(body, at);
    let (_version, flags) = bytes.full_box()?;
    let count = bytes.u32()? as usize;
    let data_offset = if flags & 0x01 != 0 {
        Some(i64::from(bytes.i32()?))
    } else {
        None
    };
    let first_flags = if flags & 0x04 != 0 {
        Some(bytes.u32()?)
    } else {
        None
    };
    // Every optional per-sample field the flags turned on, so the
    // entries a table claims are checked against the bytes it has
    // before a single one is allocated.
    let mut width = 0usize;
    for bit in [0x100u32, 0x200, 0x400, 0x800] {
        if flags & bit != 0 {
            width += 4;
        }
    }
    if count > limits.max_samples {
        return Err(Error::format("more samples than this reader will hold")
            .at(at)
            .in_box(*b"trun"));
    }
    if count
        .checked_mul(width.max(1))
        .is_none_or(|want| want > bytes.left())
    {
        return Err(Error::format("a trun longer than the box holding it")
            .at(at)
            .in_box(*b"trun"));
    }

    let duration_default = tfhd.default_duration.unwrap_or(trex.duration);
    let size_default = tfhd.default_size.unwrap_or(trex.size);
    let flags_default = tfhd.default_flags.unwrap_or(trex.flags);
    let mut out = crate::reserved(count, "a trun larger than memory")?;
    for index in 0..count {
        let duration = if flags & 0x100 != 0 {
            bytes.u32()?
        } else {
            duration_default
        };
        let size = if flags & 0x200 != 0 {
            bytes.u32()?
        } else {
            size_default
        };
        let own = if flags & 0x400 != 0 {
            Some(bytes.u32()?)
        } else {
            None
        };
        // Version 0 is unsigned on paper and signed in every file
        // ffmpeg wrote; see this module's head.
        let composition_offset = if flags & 0x800 != 0 { bytes.i32()? } else { 0 };
        out.push(Entry {
            duration,
            size,
            flags: match (index, first_flags, own) {
                (0, Some(first), _) => first,
                (_, _, Some(own)) => own,
                _ => flags_default,
            },
            composition_offset,
        });
    }
    Ok((data_offset, out))
}

/// What one `moof` said, for the track a reader is after.
struct Read {
    samples: Vec<Sample>,
    /// Where the data of the last track fragment ended, which is where
    /// the next one carries on from under §8.8.7's third case.
    end: u64,
}

/// The samples of one `moof` body, for one track or for whichever
/// track fragments the `moof` holds.
///
/// `decode` is the running decode time, carried across fragments and
/// reset by any `tfdt` for the track in question. `moof_start` is
/// where the `moof` box begins, which is the base §8.8.7 falls back
/// to; `body_at` is where its body begins, for errors.
#[allow(clippy::too_many_arguments)]
pub(crate) fn samples_of_moof(
    moof_body: &[u8],
    moof_start: u64,
    body_at: u64,
    want: Option<u32>,
    defaults: &TrexDefaults,
    decode: &mut i64,
    limits: &Limits,
) -> Result<Vec<Sample>> {
    let mut out = Vec::new();
    let mut previous_end = moof_start;
    let mut first = true;
    for traf in boxes::kids(moof_body, body_at, limits)?
        .iter()
        .filter(|item| item.is(b"traf"))
    {
        let read = read_traf(
            traf,
            moof_start,
            first,
            previous_end,
            want,
            defaults,
            decode,
            limits,
        )?;
        previous_end = read.end;
        first = false;
        out.extend(read.samples);
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn read_traf(
    traf: &Child<'_>,
    moof_start: u64,
    first: bool,
    previous_end: u64,
    want: Option<u32>,
    defaults: &TrexDefaults,
    decode: &mut i64,
    limits: &Limits,
) -> Result<Read> {
    let inner = boxes::kids(traf.body, traf.at, limits)?;
    let tfhd = boxes::child_of(&inner, b"tfhd")
        .ok_or_else(|| Error::format("a traf with no tfhd").at(traf.at))?;
    let tfhd = read_tfhd(tfhd.body, tfhd.at)?;
    let mine = want.is_none_or(|id| id == tfhd.track_id);

    // A track fragment of another track still moves the place the next
    // one starts from, so its runs are read even when its samples are
    // not wanted.
    let mut own_decode = 0i64;
    let clock: &mut i64 = if mine { decode } else { &mut own_decode };
    if let Some(tfdt) = boxes::child_of(&inner, b"tfdt") {
        *clock = read_tfdt(tfdt.body, tfdt.at)?;
    }

    let base = tfhd.base(moof_start, first, previous_end);
    let mut at = base;
    let mut samples = Vec::new();
    for trun in inner.iter().filter(|item| item.is(b"trun")) {
        let (data_offset, entries) = read_trun(trun.body, trun.at, &tfhd, defaults, limits)?;
        // §8.8.8: a run with an offset is placed from the base, and one
        // without carries on from where the last run's data ended.
        if let Some(offset) = data_offset {
            at = base.checked_add_signed(offset).ok_or_else(|| {
                Error::format("a trun whose data offset leaves the file").at(trun.at)
            })?;
        }
        for entry in entries {
            if mine {
                samples.push(Sample {
                    index: 0,
                    offset: at,
                    size: entry.size,
                    dts: *clock,
                    pts: clock.saturating_add(i64::from(entry.composition_offset)),
                    duration: i64::from(entry.duration),
                    keyframe: entry.flags & NON_SYNC == 0,
                });
            }
            at = at
                .checked_add(u64::from(entry.size))
                .ok_or_else(|| Error::format("a sample that runs past the file").at(trun.at))?;
            *clock = clock.saturating_add(i64::from(entry.duration));
        }
        if samples.len() > limits.max_samples {
            return Err(Error::format("more samples than this reader will hold"));
        }
    }
    Ok(Read { samples, end: at })
}

/// The samples of one complete `moof`+`mdat` fragment, with `offset`
/// counted from the stream position `at`, which is where the
/// fragment's first byte sits.
///
/// A fragment with no `tfdt` states no decode time of its own, so its
/// samples are timed from zero; a caller reading a stream of such
/// fragments has to carry the clock itself.
pub fn samples_of_fragment(fragment: &[u8], at: u64, track: &Track) -> Result<Vec<Sample>> {
    let limits = Limits::default();
    let header = boxes::parse_header(fragment, at)?
        .ok_or_else(|| Error::truncated("a fragment shorter than a box header").at(at))?;
    if !header.is(b"moof") {
        return Err(Error::format("a fragment that does not open with a moof")
            .at(at)
            .in_box(header.kind));
    }
    let size = match header.size {
        crate::BoxSize::Exact(size) => {
            usize::try_from(size).map_err(|_| Error::format("a moof wider than memory").at(at))?
        }
        crate::BoxSize::ToEnd => fragment.len(),
    };
    if size > fragment.len() {
        return Err(Error::truncated("a moof that overruns its fragment").at(at));
    }
    let head = usize::try_from(header.header_len).unwrap_or(usize::MAX);
    let mut decode = 0i64;
    let mut samples = samples_of_moof(
        &fragment[head..size],
        at,
        at + header.header_len,
        Some(track.track_id),
        &track.defaults,
        &mut decode,
        &limits,
    )?;
    for (index, sample) in samples.iter_mut().enumerate() {
        sample.index = index as u32;
    }
    Ok(samples)
}

/// One sample's bytes out of the fragment that carries them.
///
/// `at` is where `fragment[0]` sits in the stream, the same number
/// passed to [`Track::fragment_samples_at`]. A sample whose run was
/// placed outside the fragment, which an explicit `base_data_offset`
/// in a standalone segment can do, is refused rather than read from
/// the wrong place.
///
/// [`Track::fragment_samples_at`]: crate::track::Track::fragment_samples_at
pub fn sample_bytes<'a>(fragment: &'a [u8], at: u64, sample: &Sample) -> Result<&'a [u8]> {
    let from = sample
        .offset
        .checked_sub(at)
        .and_then(|from| usize::try_from(from).ok())
        .ok_or_else(|| {
            Error::format("a sample placed before the fragment that carries it").at(sample.offset)
        })?;
    let to = from
        .checked_add(sample.size as usize)
        .ok_or_else(|| Error::format("a sample wider than memory").at(sample.offset))?;
    fragment
        .get(from..to)
        .ok_or_else(|| Error::truncated("a sample that overruns its fragment").at(sample.offset))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxes::{boxed, full_boxed};
    use crate::track::{Handler, SampleEntry, Track};

    /// A `trun` of one sample per size given, with per-sample size and
    /// flags and nothing else.
    fn trun(data_offset: Option<i32>, sizes: &[u32]) -> Vec<u8> {
        let mut flags = 0x200u32;
        if data_offset.is_some() {
            flags |= 0x01;
        }
        let mut payload = (sizes.len() as u32).to_be_bytes().to_vec();
        if let Some(offset) = data_offset {
            payload.extend_from_slice(&offset.to_be_bytes());
        }
        for size in sizes {
            payload.extend_from_slice(&size.to_be_bytes());
        }
        full_boxed(b"trun", 0, flags, &payload)
    }

    fn tfhd(track_id: u32, flags: u32, extra: &[u8]) -> Vec<u8> {
        let mut payload = track_id.to_be_bytes().to_vec();
        payload.extend_from_slice(extra);
        full_boxed(b"tfhd", 0, flags, &payload)
    }

    fn moof(trafs: &[Vec<u8>]) -> Vec<u8> {
        let mut body = full_boxed(b"mfhd", 0, 0, &1u32.to_be_bytes());
        for traf in trafs {
            body.extend_from_slice(traf);
        }
        boxed(b"moof", &body)
    }

    fn offsets(moof_bytes: &[u8], want: u32) -> Vec<u64> {
        let head = 8usize;
        let mut decode = 0i64;
        samples_of_moof(
            &moof_bytes[head..],
            0,
            head as u64,
            Some(want),
            &TrexDefaults::default(),
            &mut decode,
            &Limits::default(),
        )
        .expect("the samples")
        .iter()
        .map(|sample| sample.offset)
        .collect()
    }

    #[test]
    fn an_explicit_base_data_offset_is_where_the_data_is() {
        let traf = boxed(
            b"traf",
            &[
                tfhd(1, 0x01, &5000u64.to_be_bytes()),
                trun(Some(0), &[10, 20]),
            ]
            .concat(),
        );
        assert_eq!(offsets(&moof(&[traf]), 1), vec![5000, 5010]);
    }

    #[test]
    fn default_base_is_moof_puts_every_track_fragment_at_the_moof() {
        // Two track fragments, both flagged default-base-is-moof: both
        // start from the moof and each states its own data offset.
        let one = boxed(
            b"traf",
            &[tfhd(1, 0x02_0000, &[]), trun(Some(200), &[10, 10])].concat(),
        );
        let two = boxed(
            b"traf",
            &[tfhd(2, 0x02_0000, &[]), trun(Some(300), &[7])].concat(),
        );
        let bytes = moof(&[one, two]);
        assert_eq!(offsets(&bytes, 1), vec![200, 210]);
        assert_eq!(offsets(&bytes, 2), vec![300]);
    }

    #[test]
    fn a_later_track_fragment_with_no_flags_carries_on_from_the_one_before() {
        // Neither flag set. The first track fragment starts at the moof
        // and runs 30 bytes; the second must start where that ended,
        // not back at the moof. The two old copies of this code either
        // put it back at the moof or refused the file.
        let one = boxed(b"traf", &[tfhd(1, 0, &[]), trun(None, &[10, 20])].concat());
        let two = boxed(b"traf", &[tfhd(2, 0, &[]), trun(None, &[5, 5])].concat());
        let bytes = moof(&[one, two]);
        assert_eq!(offsets(&bytes, 1), vec![0, 10]);
        assert_eq!(offsets(&bytes, 2), vec![30, 35]);
    }

    #[test]
    fn a_run_with_no_offset_carries_on_from_the_run_before_it() {
        // §8.8.8: the second trun has no data_offset, so its data
        // starts where the first run's ended rather than back at the
        // base.
        let traf = boxed(
            b"traf",
            &[
                tfhd(1, 0x02_0000, &[]),
                trun(Some(100), &[10, 10]),
                trun(None, &[4]),
            ]
            .concat(),
        );
        assert_eq!(offsets(&moof(&[traf]), 1), vec![100, 110, 120]);
    }

    #[test]
    fn a_version_zero_composition_offset_is_read_as_signed() {
        // flags: data-offset, sample-size, sample-composition-offset.
        let mut payload = 1u32.to_be_bytes().to_vec();
        payload.extend_from_slice(&0i32.to_be_bytes());
        payload.extend_from_slice(&8u32.to_be_bytes());
        payload.extend_from_slice(&(-1024i32).to_be_bytes());
        let body = full_boxed(b"trun", 0, 0x01 | 0x200 | 0x800, &payload);
        let (_, entries) = read_trun(
            &body[8..],
            0,
            &read_tfhd(&tfhd(1, 0, &[])[8..], 0).expect("a tfhd"),
            &TrexDefaults::default(),
            &Limits::default(),
        )
        .expect("the entries");
        assert_eq!(entries[0].composition_offset, -1024);
    }

    #[test]
    fn a_trun_claiming_more_entries_than_it_holds_is_refused() {
        let mut payload = 1_000_000u32.to_be_bytes().to_vec();
        payload.extend_from_slice(&[0; 8]);
        let body = full_boxed(b"trun", 0, 0x200 | 0x100, &payload);
        let err = read_trun(
            &body[8..],
            77,
            &read_tfhd(&tfhd(1, 0, &[])[8..], 0).expect("a tfhd"),
            &TrexDefaults::default(),
            &Limits::default(),
        )
        .expect_err("a refusal");
        assert_eq!(err.offset(), Some(77));
    }

    #[test]
    fn a_fragment_that_does_not_open_with_a_moof_is_refused() {
        let track = Track {
            track_id: 1,
            handler: Handler::Video,
            timescale: 30,
            movie_timescale: 1000,
            entry: SampleEntry::default(),
            defaults: TrexDefaults::default(),
            edit: crate::track::Edit::default(),
            samples: Vec::new(),
            start_shift: 0,
            next_dts: 0,
        };
        let err = track
            .fragment_samples(&boxed(b"free", &[]))
            .expect_err("a refusal");
        assert!(err.to_string().contains("moof"), "{err}");
    }
}

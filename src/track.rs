//! A track, its sample tables, and its edit list.
//!
//! What a `moov` says about one track (§8.3.1): which handler it is
//! (§8.4.3), what unit its times are counted in (§8.4.2), what its
//! samples are coded as and what out-of-band header they need
//! (§8.5.2), what defaults its fragments fall back to (§8.8.3), and,
//! where the track is not fragmented, where every sample is and when it
//! is shown (§8.6, §8.7).
//!
//! # The edit list
//!
//! ffprobe prints a packet's `pts_time` after the `elst` (§8.6.6) has
//! moved it, so this does the same thing, in one rule:
//!
//! > A sample's composition time is `dts + ctts`, with `dts` running
//! > from zero through the `stts` deltas. The file's zero is the
//! > composition time of the first sample, in decode order, whose
//! > composition time is at or after the `media_time` of the first
//! > edit that is not empty. Presentation time is a sample's
//! > composition time minus that zero, plus the durations of the empty
//! > edits (`media_time` -1) in front of it, rescaled from the movie
//! > timescale to the media one. A file with no `elst` keeps its
//! > composition times as they are.
//!
//! The snap in the middle of that is the part worth saying twice.
//! ffmpeg does not subtract `media_time`; it drops the samples before
//! it and makes the first one it keeps zero. For every file where
//! `media_time` lands exactly on a sample, which is every ordinary
//! encode, the two are the same number. They part company when it does
//! not: raw H.264 remuxed into MP4 gets a `media_time` of two frames at
//! a nominal rate while the samples themselves are a tick shorter, and
//! the difference there is 20 milliseconds, which is most of a frame.
//! A sample before the zero keeps its negative presentation time here
//! rather than being dropped, which is also what ffprobe prints for it.
//!
//! What is left out: an edit list of several non-empty entries, which
//! ffmpeg implements by cutting and repeating samples. Nothing a muxer
//! writes has one, and guessing at it would be worse than saying so, so
//! the first non-empty entry decides and the rest are ignored.
//!
//! **This crate's own writer writes no `elst` at all** ([`crate::mux`]),
//! so a file it wrote is read back here with `start_shift` of zero and
//! its composition times exactly as stored. For a stream that reorders
//! frames that means the first sample is presented at its composition
//! offset rather than at zero, which is what ffprobe says about
//! ffmpeg's own `frag_keyframe+empty_moov` output too.
//!
//! The box nesting walked here is a fixed path, so there is no
//! recursion to bound: `moov` and each `moof` are read once, whole, and
//! their children are walked flat.

use std::io::{Read, Seek};

use crate::boxes::{self, child, kids, Bytes, Child};
use crate::fragment;
use crate::source::Source;
use crate::time;
use crate::{reserved, Error, Limits, Result};

/// Where a sample entry's child boxes start: past the eight bytes of
/// box header, the eight of `SampleEntry` (§8.5.2), and the seventy of
/// `VisualSampleEntry` (§12.1.3).
const VISUAL_SAMPLE_ENTRY_LEN: usize = 86;

/// Where an `AudioSampleEntry`'s child boxes start, counted the same
/// way (§12.2.3).
const AUDIO_SAMPLE_ENTRY_LEN: usize = 36;

/// What a track carries, by its `hdlr` (§8.4.3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handler {
    /// `vide`.
    Video,
    /// `soun`.
    Audio,
    /// Anything else, kept as it was written.
    Other([u8; 4]),
}

impl Handler {
    pub fn of(kind: [u8; 4]) -> Self {
        match &kind {
            b"vide" => Handler::Video,
            b"soun" => Handler::Audio,
            _ => Handler::Other(kind),
        }
    }

    pub fn code(self) -> [u8; 4] {
        match self {
            Handler::Video => *b"vide",
            Handler::Audio => *b"soun",
            Handler::Other(kind) => kind,
        }
    }
}

/// The picture a visual sample entry declares (§12.1.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Visual {
    pub width: u32,
    pub height: u32,
}

/// The sound an audio sample entry declares (§12.2.3). The sample rate
/// is the 16.16 field's whole part, which is all a rate under 65536
/// needs; the real one is in the decoder configuration either way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Audio {
    pub sample_rate: u32,
    pub channels: u32,
}

/// The first sample entry of a track's `stsd` (§8.5.2), and the
/// decoder configuration record inside it.
///
/// The record is **opaque**: `avcC` (ISO/IEC 14496-15 §5.3.3), `hvcC`
/// (§8.3.3 of the same), `av1C` (the AV1 codec ISO media file format
/// binding), or the payload of an `esds` (ISO/IEC 14496-14 §5.6), all
/// of them carried from the file to the caller as bytes. Reading them
/// is `ffrwd-nal`'s job and is deliberately not this crate's.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SampleEntry {
    /// The entry's own four characters: `avc1`, `hvc1`, `av01`,
    /// `mp4a`, and so on.
    pub kind: [u8; 4],
    /// The decoder configuration record, as the file carried it. Empty
    /// when the entry has none this crate recognises the place of.
    pub config: Vec<u8>,
    /// The picture, for a visual entry.
    pub visual: Option<Visual>,
    /// The sound, for an audio entry.
    pub audio: Option<Audio>,
}

impl SampleEntry {
    /// Which child box of the entry holds the configuration record for
    /// a given entry type, when this crate knows.
    pub fn config_box(kind: &[u8; 4]) -> Option<&'static [u8; 4]> {
        match kind {
            b"avc1" | b"avc3" => Some(b"avcC"),
            b"hvc1" | b"hev1" => Some(b"hvcC"),
            b"av01" => Some(b"av1C"),
            b"mp4a" => Some(b"esds"),
            _ => None,
        }
    }
}

/// One coded sample: where its bytes are, when it is decoded and shown,
/// and whether a reader may start at it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    /// Its place in decode order, from zero.
    pub index: u32,
    /// Where its bytes begin. In a file read that is a file offset; in
    /// a fragment read it is an offset in the stream the fragment came
    /// from, which for a standalone fragment is an offset within it.
    pub offset: u64,
    /// How many bytes they are.
    pub size: u32,
    /// Decode time in the track's own ticks, with any edit list shift
    /// already applied.
    pub dts: i64,
    /// Presentation time in the track's own ticks, with the edit list
    /// already applied: the number ffprobe divides by the time base to
    /// print `pts_time`.
    pub pts: i64,
    /// How long it is shown, in the track's own ticks. Zero where the
    /// file did not say.
    pub duration: i64,
    /// Whether the sample is a random access point.
    pub keyframe: bool,
}

/// What a track's `elst` amounts to, before the samples are known
/// (§8.6.6).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Edit {
    /// The empty edits in front, in media ticks: how much later
    /// everything is shown.
    pub empty: i64,
    /// The `media_time` of the first edit that shows something, or
    /// `None` when there is no edit list to apply.
    pub media_time: Option<i64>,
}

/// The defaults a `trex` sets for every fragment of a track (§8.8.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TrexDefaults {
    pub duration: u32,
    pub size: u32,
    pub flags: u32,
}

/// One track of a movie (§8.3.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Track {
    /// `tkhd`'s `track_ID` (§8.3.2).
    pub track_id: u32,
    /// What the track carries (§8.4.3).
    pub handler: Handler,
    /// Ticks a second, the denominator ffprobe calls the time base
    /// (§8.4.2).
    pub timescale: u32,
    /// The movie timescale the file declared (§8.2.2), which is what
    /// an edit list's durations are counted in.
    pub movie_timescale: u32,
    /// The first sample entry, and the decoder configuration in it.
    pub entry: SampleEntry,
    /// The fragment defaults (§8.8.3).
    pub defaults: TrexDefaults,
    /// What the edit list amounts to (§8.6.6).
    pub edit: Edit,
    /// Every sample, in decode order. Empty for an init segment, whose
    /// sample tables are all zero by construction.
    pub samples: Vec<Sample>,
    /// How far the edit list moved every time: `pts` as returned is the
    /// stored composition time plus this.
    pub start_shift: i64,
}

impl Track {
    /// A track assembled from parts rather than parsed out of boxes.
    ///
    /// Every field of [`Track`] is public, so a struct literal would
    /// work too; this exists so that a caller need not name the fields
    /// that only an ISO file has, and so that adding one later is not a
    /// breaking change. It is what a reader of **another container**
    /// uses to hand back the same type an MP4 read produces, which is
    /// what lets one sample-reading routine serve both.
    ///
    /// The fields it does not take are left at the values that mean
    /// "the file said nothing": `track_id` 1, `movie_timescale`
    /// [`DEFAULT_MOVIE_TIMESCALE`], an [`Edit`] with no entries,
    /// [`TrexDefaults`] all zero, and `start_shift` 0. Set any of them
    /// afterwards, or use [`with_track_id`](Self::with_track_id) and
    /// [`with_start_shift`](Self::with_start_shift).
    ///
    /// # What the caller is promising
    ///
    /// - `samples` is in **decode order**. `index` is renumbered here
    ///   from zero, so the caller does not have to get that right.
    /// - Each sample's `offset` and `size` are **absolute in whatever
    ///   byte source the samples will be read from**, the same source a
    ///   [`Source`] will be opened on. They are not relative to a
    ///   cluster, a block or a fragment.
    /// - `dts`, `pts` and `duration` are in **`timescale` ticks**, with
    ///   any shift the container applies already applied, so that
    ///   [`ms`](Self::ms) prints what a player shows. `start_shift`
    ///   records what that shift was, for a caller that wants to say
    ///   why; nothing in this crate subtracts it again.
    /// - `keyframe` is true exactly where a decoder may start.
    /// - `duration` may be 0 where the container did not say. Nothing
    ///   here divides by it.
    ///
    /// A `timescale` of 0 is taken as 1, because dividing by it is
    /// worse than not.
    ///
    /// [`DEFAULT_MOVIE_TIMESCALE`]: crate::mux::DEFAULT_MOVIE_TIMESCALE
    /// [`Source`]: crate::source::Source
    pub fn from_parts(
        handler: Handler,
        timescale: u32,
        entry: SampleEntry,
        samples: Vec<Sample>,
    ) -> Track {
        let mut samples = samples;
        for (index, sample) in samples.iter_mut().enumerate() {
            sample.index = index as u32;
        }
        Track {
            track_id: 1,
            handler,
            timescale: timescale.max(1),
            movie_timescale: crate::mux::DEFAULT_MOVIE_TIMESCALE,
            entry,
            defaults: TrexDefaults::default(),
            edit: Edit::default(),
            samples,
            start_shift: 0,
        }
    }

    /// The `track_ID` this track answers to, which is what
    /// [`fragment_samples`](Self::fragment_samples) matches a `traf`
    /// against.
    pub fn with_track_id(mut self, track_id: u32) -> Self {
        self.track_id = track_id;
        self
    }

    /// What a container's own start offset moved every time by, for a
    /// caller that wants to report it. The sample times are expected to
    /// have it in them already.
    pub fn with_start_shift(mut self, start_shift: i64) -> Self {
        self.start_shift = start_shift;
        self
    }

    /// A tick count of this track's timescale, as milliseconds.
    pub fn ms(&self, ticks: i64) -> i64 {
        time::ms(ticks, self.timescale)
    }

    /// The decode time after the last sample, which is where a fragment
    /// that states no `tfdt` of its own carries on from.
    ///
    /// Derived rather than stored: it is the last sample's `dts` plus
    /// its `duration`, and zero for a track with no samples. For a
    /// track read out of a `moov` that is exactly what the `stts`
    /// deltas add up to.
    pub fn next_dts(&self) -> i64 {
        match self.samples.last() {
            Some(last) => last.dts.saturating_add(last.duration),
            None => 0,
        }
    }

    pub fn is_video(&self) -> bool {
        self.handler == Handler::Video
    }

    pub fn is_audio(&self) -> bool {
        self.handler == Handler::Audio
    }

    /// Every track a `moov` body describes, with the default limits.
    pub fn all(moov: &[u8]) -> Result<Vec<Track>> {
        Self::all_bounded(moov, 0, &Limits::default())
    }

    /// The same, placed in the file and bounded by a caller's limits.
    pub fn all_bounded(moov: &[u8], at: u64, limits: &Limits) -> Result<Vec<Track>> {
        let top = kids(moov, at, limits)?;
        let movie_timescale = match child(&top, b"mvhd") {
            // A movie timescale of zero says nothing, and it only ever
            // scales an edit list's durations, so the default stands in
            // rather than the file being refused for it.
            Some(body) => mvhd_timescale(body)?,
            None => 1000,
        };
        let mut out = Vec::new();
        for entry in top.iter().filter(|item| item.is(b"trak")) {
            if let Some(track) = read_trak(entry, movie_timescale, &top, limits)? {
                out.push(track);
            }
        }
        Ok(out)
    }

    /// The one track of an init segment: the `ftyp`+`moov` a decoder
    /// reads before any fragment.
    ///
    /// An init segment with several tracks is refused by name rather
    /// than having one of them picked silently.
    pub fn from_init(init: &[u8]) -> Result<Track> {
        Self::from_init_bounded(init, &Limits::default())
    }

    /// The same, bounded by a caller's limits.
    pub fn from_init_bounded(init: &[u8], limits: &Limits) -> Result<Track> {
        let moov = boxes::find_in(init, b"moov", 0)?
            .ok_or_else(|| Error::format("the init segment carries no moov"))?;
        let mut tracks = Track::all_bounded(moov, 0, limits)?;
        match tracks.len() {
            1 => Ok(tracks.remove(0)),
            0 => Err(Error::format("the moov carries no trak")),
            _ => Err(Error::unsupported(
                "the init segment carries several tracks, and this call is for one",
            )),
        }
    }

    /// The samples of one complete `moof`+`mdat` fragment, in decode
    /// order, with `offset` counted from the first byte of `fragment`.
    ///
    /// See [`crate::fragment`] for how a sample's position is worked
    /// out, which is the part of §8.8.7 implementations disagree about.
    ///
    /// This reads the `moof` against this track's `track_id` and
    /// `defaults`, and nothing else about the track is consulted. On a
    /// track from [`from_parts`](Self::from_parts) that means `track_id`
    /// 1 and no `trex` defaults, so a `moof` naming another track comes
    /// back empty and one that relies on `trex` defaults reads them as
    /// zero. A track that was not built from an ISO init segment is not
    /// going to be handed ISO fragments, and this says what happens
    /// rather than refusing.
    pub fn fragment_samples(&self, fragment: &[u8]) -> Result<Vec<Sample>> {
        self.fragment_samples_at(fragment, 0)
    }

    /// The same, for a fragment whose first byte sits at `at` in the
    /// stream it was cut from, so that an explicit `base_data_offset`
    /// resolves to the offsets the rest of the stream uses.
    pub fn fragment_samples_at(&self, fragment: &[u8], at: u64) -> Result<Vec<Sample>> {
        fragment::samples_of_fragment(fragment, at, self)
    }
}

/// Which track a read is after.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    /// The first track whose handler is `vide`.
    Video,
    /// The first track whose handler is `soun`.
    Audio,
    /// The track with this `track_ID`.
    Id(u32),
    /// Whichever track comes first.
    First,
}

impl Pick {
    fn wants(&self, track: &Track) -> bool {
        match self {
            Pick::Video => track.is_video(),
            Pick::Audio => track.is_audio(),
            Pick::Id(id) => track.track_id == *id,
            Pick::First => true,
        }
    }
}

/// One track of a file, with every sample of it.
///
/// Fragmented and not are the same call: the `moov` gives the timing
/// and the sample entry, its sample tables give whatever samples it
/// holds, and each `moof` after it adds its own. The edit list is
/// applied last, over all of them.
///
/// No sample's bytes are read. What comes back is where they are.
pub fn read<R: Read + Seek>(src: &mut Source<R>, pick: Pick) -> Result<Track> {
    let limits = *src.limits();
    let top = boxes::top_level(src)?;
    let moov = top
        .iter()
        .find(|header| header.is(b"moov"))
        .ok_or_else(|| Error::format("the file has no moov box"))?;
    let body = src.span(moov.body, moov.body_len())?;
    let mut track = Track::all_bounded(&body, moov.body, &limits)?
        .into_iter()
        .find(|track| pick.wants(track))
        .ok_or_else(|| {
            Error::unsupported("the file holds no track of the kind this read was after")
        })?;

    let mut decode = track.next_dts();
    for moof in top.iter().filter(|header| header.is(b"moof")) {
        let bytes = src.span(moof.body, moof.body_len())?;
        let found = fragment::samples_of_moof(
            &bytes,
            moof.start,
            moof.body,
            Some(track.track_id),
            &track.defaults,
            &mut decode,
            &limits,
        )?;
        track.samples.extend(found);
        if track.samples.len() > limits.max_samples {
            return Err(Error::format("more samples than this reader will hold"));
        }
    }

    apply_edit(&mut track);
    Ok(track)
}

/// The edit list, over whatever samples the track ended up with.
pub(crate) fn apply_edit(track: &mut Track) {
    // Where the file's zero is: the first sample the edit list keeps,
    // which for a file with no edit list is composition time zero.
    let zero = match track.edit.media_time {
        None => 0,
        Some(media_time) => track
            .samples
            .iter()
            .find(|sample| sample.pts >= media_time)
            .map_or(media_time, |sample| sample.pts),
    };
    let shift = track.edit.empty.saturating_sub(zero);
    for (index, sample) in track.samples.iter_mut().enumerate() {
        sample.index = index as u32;
        sample.pts = sample.pts.saturating_add(shift);
        sample.dts = sample.dts.saturating_add(shift);
    }
    track.start_shift = shift;
}

// ------------------------------------------------------------------ //
// The moov.
// ------------------------------------------------------------------ //

fn read_trak(
    entry: &Child<'_>,
    movie_timescale: u32,
    moov: &[Child<'_>],
    limits: &Limits,
) -> Result<Option<Track>> {
    let trak = kids(entry.body, entry.at, limits)?;
    let mdia = match child(&trak, b"mdia") {
        Some(body) => kids(body, 0, limits)?,
        None => return Ok(None),
    };
    let mdhd = child(&mdia, b"mdhd").ok_or_else(|| Error::format("a track with no mdhd"))?;
    let timescale = mdhd_timescale(mdhd)?;
    let handler = match child(&mdia, b"hdlr") {
        Some(body) => handler_of(body)?,
        None => Handler::Other([0; 4]),
    };
    let track_id = child(&trak, b"tkhd")
        .map(tkhd_track_id)
        .transpose()?
        .unwrap_or(1);
    let edit = match child(&trak, b"edts")
        .map(|body| kids(body, 0, limits))
        .transpose()?
    {
        Some(edts) => match child(&edts, b"elst") {
            Some(elst) => edit_of(elst, movie_timescale, timescale)?,
            None => Edit::default(),
        },
        None => Edit::default(),
    };

    let minf = kids(
        child(&mdia, b"minf").ok_or_else(|| Error::format("a track with no minf"))?,
        0,
        limits,
    )?;
    let stbl = kids(
        child(&minf, b"stbl").ok_or_else(|| Error::format("a track with no stbl"))?,
        0,
        limits,
    )?;
    let stsd = child(&stbl, b"stsd").ok_or_else(|| Error::format("a track with no stsd"))?;
    let entry = sample_entry(stsd, limits)?;
    let samples = samples_of_stbl(&stbl, limits)?;
    let defaults = trex_of(moov, track_id, limits)?;
    Ok(Some(Track {
        track_id,
        handler,
        timescale,
        movie_timescale,
        entry,
        defaults,
        edit,
        samples,
        start_shift: 0,
    }))
}

fn handler_of(hdlr: &[u8]) -> Result<Handler> {
    let mut bytes = Bytes::new(hdlr);
    bytes.full_box()?;
    bytes.skip(4)?;
    let code = bytes.take(4)?;
    Ok(Handler::of([code[0], code[1], code[2], code[3]]))
}

/// The movie timescale (§8.2.2). A zero is the file saying nothing, so
/// the usual default stands in.
fn mvhd_timescale(body: &[u8]) -> Result<u32> {
    let mut bytes = Bytes::new(body);
    let (version, _) = bytes.full_box()?;
    bytes.skip(if version == 1 { 16 } else { 8 })?;
    let declared = bytes.u32()?;
    Ok(if declared == 0 { 1000 } else { declared })
}

/// The media timescale (§8.4.2). A zero here is refused: it is the
/// divisor of every time the track has, and a reader that quietly
/// substitutes one is a reader whose timestamps mean nothing.
fn mdhd_timescale(body: &[u8]) -> Result<u32> {
    let mut bytes = Bytes::new(body);
    let (version, _) = bytes.full_box()?;
    bytes.skip(if version == 1 { 16 } else { 8 })?;
    let timescale = bytes.u32()?;
    if timescale == 0 {
        return Err(Error::format("a track whose timescale is zero").in_box(*b"mdhd"));
    }
    Ok(timescale)
}

fn tkhd_track_id(body: &[u8]) -> Result<u32> {
    let mut bytes = Bytes::new(body);
    let (version, _) = bytes.full_box()?;
    bytes.skip(if version == 1 { 16 } else { 8 })?;
    bytes.u32()
}

/// The edit list, as far as the rule at the top of this file uses it
/// (§8.6.6).
fn edit_of(elst: &[u8], movie_timescale: u32, media_timescale: u32) -> Result<Edit> {
    let mut bytes = Bytes::new(elst);
    let (version, _) = bytes.full_box()?;
    let entry_size = if version == 1 { 20 } else { 12 };
    let count = bytes.count(entry_size)?;
    let mut empty = 0i64;
    for _ in 0..count {
        let (duration, media_time) = if version == 1 {
            (bytes.u64()? as i64, bytes.u64()? as i64)
        } else {
            (i64::from(bytes.u32()?), i64::from(bytes.i32()?))
        };
        bytes.skip(4)?;
        if media_time < 0 {
            // An empty edit: the file shows nothing for this long, so
            // everything after it is that much later.
            empty = empty.saturating_add(time::rescale(
                duration,
                u64::from(movie_timescale),
                u64::from(media_timescale),
            ));
            continue;
        }
        // The first edit that shows something decides where zero is.
        return Ok(Edit {
            empty,
            media_time: Some(media_time),
        });
    }
    Ok(Edit {
        empty,
        media_time: None,
    })
}

/// The first sample entry of an `stsd`, and the configuration record
/// inside it (§8.5.2).
fn sample_entry(stsd: &[u8], limits: &Limits) -> Result<SampleEntry> {
    let mut bytes = Bytes::new(stsd);
    bytes.full_box()?;
    let count = bytes.count(8)?;
    if count == 0 {
        return Err(Error::format("an stsd with no sample entry").in_box(*b"stsd"));
    }
    let entries = kids(bytes.rest(), 0, limits)?;
    let entry = entries
        .first()
        .ok_or_else(|| Error::format("an stsd with no sample entry").in_box(*b"stsd"))?;
    let kind = entry.kind;
    let (fixed, visual, audio) = match SampleEntry::config_box(&kind) {
        Some(b"esds") => {
            let sound = audio_fields(entry.body)?;
            (AUDIO_SAMPLE_ENTRY_LEN, None, Some(sound))
        }
        Some(_) => {
            let picture = visual_fields(entry.body)?;
            (VISUAL_SAMPLE_ENTRY_LEN, Some(picture), None)
        }
        // An entry this crate does not know the shape of still names
        // itself; the caller decides whether it can use it.
        None => {
            return Ok(SampleEntry {
                kind,
                config: Vec::new(),
                visual: None,
                audio: None,
            })
        }
    };
    // The entry's own header is already off the body, so the fixed
    // fields that remain are eight fewer than the shape's own length.
    let inner = entry.body.get(fixed - 8..).ok_or_else(|| {
        Error::truncated("a sample entry shorter than its own fields").in_box(kind)
    })?;
    let wanted = SampleEntry::config_box(&kind).expect("a known entry has a config box");
    let config = match child(&kids(inner, 0, limits)?, wanted) {
        // An `esds` is a FullBox, and what a caller wants out of it is
        // the descriptor chain, not the version and flags.
        Some(body) if wanted == b"esds" => body.get(4..).unwrap_or_default().to_vec(),
        Some(body) => body.to_vec(),
        None => Vec::new(),
    };
    Ok(SampleEntry {
        kind,
        config,
        visual,
        audio,
    })
}

/// `VisualSampleEntry`'s declared frame size (§12.1.3).
fn visual_fields(entry: &[u8]) -> Result<Visual> {
    let mut bytes = Bytes::new(entry);
    // SampleEntry: six reserved bytes and a data_reference_index, then
    // sixteen of pre_defined and reserved.
    bytes.skip(8 + 16)?;
    Ok(Visual {
        width: u32::from(bytes.u16()?),
        height: u32::from(bytes.u16()?),
    })
}

/// `AudioSampleEntry`'s channel count and sample rate (§12.2.3).
fn audio_fields(entry: &[u8]) -> Result<Audio> {
    let mut bytes = Bytes::new(entry);
    bytes.skip(8 + 8)?;
    let channels = u32::from(bytes.u16()?);
    bytes.skip(2 + 4)?;
    // A 16.16 fixed-point rate: the whole part is the one a caller
    // wants, and a rate past 65535 is zero here and right in the
    // decoder configuration.
    let sample_rate = u32::from(bytes.u16()?);
    Ok(Audio {
        sample_rate,
        channels,
    })
}

fn trex_of(moov: &[Child<'_>], track_id: u32, limits: &Limits) -> Result<TrexDefaults> {
    let Some(mvex) = child(moov, b"mvex") else {
        return Ok(TrexDefaults::default());
    };
    for entry in kids(mvex, 0, limits)?
        .iter()
        .filter(|item| item.is(b"trex"))
    {
        let mut bytes = Bytes::new(entry.body);
        bytes.full_box()?;
        if bytes.u32()? != track_id {
            continue;
        }
        bytes.skip(4)?;
        return Ok(TrexDefaults {
            duration: bytes.u32()?,
            size: bytes.u32()?,
            flags: bytes.u32()?,
        });
    }
    Ok(TrexDefaults::default())
}

// ------------------------------------------------------------------ //
// The sample tables.
// ------------------------------------------------------------------ //

/// Every sample a `stbl` describes, in decode order (§8.5.1).
///
/// The decode time after the last of them, which is where a fragment
/// with no `tfdt` carries on from, is [`Track::next_dts`]: the last
/// sample's own decode time plus its duration, which is what the `stts`
/// deltas add up to.
pub(crate) fn samples_of_stbl(stbl: &[Child<'_>], limits: &Limits) -> Result<Vec<Sample>> {
    let sizes = match child(stbl, b"stsz") {
        Some(body) => stsz(body, limits)?,
        None => match child(stbl, b"stz2") {
            Some(body) => stz2(body, limits)?,
            None => Sizes::Uniform { size: 0, count: 0 },
        },
    };
    let total = sizes.count();
    if total > limits.max_samples {
        return Err(Error::format("more samples than this reader will hold"));
    }
    if total == 0 {
        return Ok(Vec::new());
    }
    let chunks = match child(stbl, b"stco") {
        Some(body) => offsets32(body)?,
        None => match child(stbl, b"co64") {
            Some(body) => offsets64(body)?,
            None => return Err(Error::format("a track with samples but no chunk offsets")),
        },
    };
    let runs = match child(stbl, b"stsc") {
        Some(body) => stsc(body)?,
        None => return Err(Error::format("a track with samples but no stsc")),
    };
    let times = match child(stbl, b"stts") {
        Some(body) => pairs(body)?,
        None => Vec::new(),
    };
    let composition = match child(stbl, b"ctts") {
        Some(body) => ctts(body)?,
        None => Vec::new(),
    };
    let sync = match child(stbl, b"stss") {
        Some(body) => Some(sync_numbers(body)?),
        None => None,
    };

    let mut samples: Vec<Sample> = reserved(total, "a sample table larger than memory")?;
    for (index, run) in runs.iter().enumerate() {
        let first = run.0.max(1) as usize;
        let after = match runs.get(index + 1) {
            Some(next) => (next.0.max(1) as usize).max(first),
            None => chunks.len() + 1,
        };
        for chunk in first..after {
            let Some(base) = chunks.get(chunk - 1) else {
                break;
            };
            let mut at = *base;
            for _ in 0..run.1 {
                if samples.len() >= total {
                    break;
                }
                let size = sizes.get(samples.len());
                samples.push(Sample {
                    index: samples.len() as u32,
                    offset: at,
                    size,
                    ..Sample::default()
                });
                at = at
                    .checked_add(u64::from(size))
                    .ok_or_else(|| Error::format("a chunk that runs past the file"))?;
            }
        }
        if samples.len() >= total {
            break;
        }
    }
    if samples.len() != total {
        return Err(Error::format(
            "the chunk table describes fewer samples than the size table",
        ));
    }

    // Decode times, then the composition offsets on top of them.
    let mut dts = 0i64;
    let mut at = 0usize;
    for (count, delta) in &times {
        for _ in 0..*count {
            if at >= samples.len() {
                break;
            }
            samples[at].dts = dts;
            samples[at].pts = dts;
            samples[at].duration = i64::from(*delta);
            dts = dts.saturating_add(i64::from(*delta));
            at += 1;
        }
    }
    // A stts that ran out leaves the rest of the samples at the last
    // decode time reached, which is what a damaged table deserves and
    // what ffmpeg does with one.
    for sample in samples.iter_mut().skip(at) {
        sample.dts = dts;
        sample.pts = dts;
    }
    let mut at = 0usize;
    for (count, offset) in &composition {
        for _ in 0..*count {
            if at >= samples.len() {
                break;
            }
            samples[at].pts = samples[at].pts.saturating_add(i64::from(*offset));
            at += 1;
        }
    }

    match sync {
        Some(numbers) => {
            for number in numbers {
                if let Some(sample) = samples.get_mut(number.saturating_sub(1) as usize) {
                    sample.keyframe = true;
                }
            }
        }
        // No stss at all means every sample is a sync sample (§8.6.2),
        // which is what an all-intra track looks like.
        None => samples.iter_mut().for_each(|sample| sample.keyframe = true),
    }
    Ok(samples)
}

/// A sample size table, either one number for all of them or a list
/// (§8.7.3).
#[derive(Clone, Debug)]
enum Sizes {
    Uniform { size: u32, count: usize },
    Table(Vec<u32>),
}

impl Sizes {
    fn count(&self) -> usize {
        match self {
            Sizes::Uniform { count, .. } => *count,
            Sizes::Table(list) => list.len(),
        }
    }

    fn get(&self, index: usize) -> u32 {
        match self {
            Sizes::Uniform { size, .. } => *size,
            Sizes::Table(list) => list.get(index).copied().unwrap_or(0),
        }
    }
}

fn stsz(body: &[u8], limits: &Limits) -> Result<Sizes> {
    let mut bytes = Bytes::new(body);
    bytes.full_box()?;
    let uniform = bytes.u32()?;
    if uniform != 0 {
        let count = bytes.u32()? as usize;
        if count > limits.max_samples {
            return Err(Error::format("more samples than this reader will hold"));
        }
        return Ok(Sizes::Uniform {
            size: uniform,
            count,
        });
    }
    let count = bytes.count(4)?;
    let mut list = reserved(count, "a size table larger than memory")?;
    for _ in 0..count {
        list.push(bytes.u32()?);
    }
    Ok(Sizes::Table(list))
}

fn stz2(body: &[u8], _limits: &Limits) -> Result<Sizes> {
    let mut bytes = Bytes::new(body);
    bytes.full_box()?;
    bytes.skip(3)?;
    let field = bytes.u8()?;
    if !matches!(field, 4 | 8 | 16) {
        return Err(Error::format("an stz2 field width that is not 4, 8 or 16").in_box(*b"stz2"));
    }
    let count = bytes.u32()? as usize;
    let needed = match field {
        4 => count.div_ceil(2),
        8 => count,
        _ => count
            .checked_mul(2)
            .ok_or_else(|| Error::format("a table wider than memory"))?,
    };
    let packed = bytes.take(needed)?;
    let mut list = reserved(count, "a size table larger than memory")?;
    for index in 0..count {
        list.push(match field {
            4 => {
                let byte = packed[index / 2];
                u32::from(if index % 2 == 0 {
                    byte >> 4
                } else {
                    byte & 0x0f
                })
            }
            8 => u32::from(packed[index]),
            _ => u32::from(u16::from_be_bytes([
                packed[index * 2],
                packed[index * 2 + 1],
            ])),
        });
    }
    Ok(Sizes::Table(list))
}

/// `stco`, the 32-bit chunk offsets (§8.7.5).
fn offsets32(body: &[u8]) -> Result<Vec<u64>> {
    let mut bytes = Bytes::new(body);
    bytes.full_box()?;
    let count = bytes.count(4)?;
    let mut out = reserved(count, "a chunk table larger than memory")?;
    for _ in 0..count {
        out.push(u64::from(bytes.u32()?));
    }
    Ok(out)
}

/// `co64`, the 64-bit ones (§8.7.5).
fn offsets64(body: &[u8]) -> Result<Vec<u64>> {
    let mut bytes = Bytes::new(body);
    bytes.full_box()?;
    let count = bytes.count(8)?;
    let mut out = reserved(count, "a chunk table larger than memory")?;
    for _ in 0..count {
        out.push(bytes.u64()?);
    }
    Ok(out)
}

/// `stsc`: first chunk, samples per chunk, sample description index
/// (§8.7.4).
fn stsc(body: &[u8]) -> Result<Vec<(u32, u32, u32)>> {
    let mut bytes = Bytes::new(body);
    bytes.full_box()?;
    let count = bytes.count(12)?;
    let mut out = reserved(count, "a chunk map larger than memory")?;
    for _ in 0..count {
        out.push((bytes.u32()?, bytes.u32()?, bytes.u32()?));
    }
    Ok(out)
}

/// `stts`: sample count and delta (§8.6.1.2).
fn pairs(body: &[u8]) -> Result<Vec<(u32, u32)>> {
    let mut bytes = Bytes::new(body);
    bytes.full_box()?;
    let count = bytes.count(8)?;
    let mut out = reserved(count, "a time table larger than memory")?;
    for _ in 0..count {
        out.push((bytes.u32()?, bytes.u32()?));
    }
    Ok(out)
}

/// `ctts`, the composition offsets (§8.6.1.3).
///
/// Version 1 signs them on paper and version 0 does not, and ffmpeg
/// writes negative values into a version 0 box anyway. Both are read
/// signed here, which is what ffmpeg's own reader does and what the
/// `trun` path in [`crate::fragment`] does with the matching field.
fn ctts(body: &[u8]) -> Result<Vec<(u32, i32)>> {
    let mut bytes = Bytes::new(body);
    bytes.full_box()?;
    let count = bytes.count(8)?;
    let mut out = reserved(count, "a composition table larger than memory")?;
    for _ in 0..count {
        out.push((bytes.u32()?, bytes.i32()?));
    }
    Ok(out)
}

/// `stss`, the sync sample numbers, counted from one (§8.6.2).
fn sync_numbers(body: &[u8]) -> Result<Vec<u32>> {
    let mut bytes = Bytes::new(body);
    bytes.full_box()?;
    let count = bytes.count(4)?;
    let mut out = reserved(count, "a sync table larger than memory")?;
    for _ in 0..count {
        out.push(bytes.u32()?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxes::{boxed, full_boxed};

    fn elst_v0(entries: &[(u32, i32)]) -> Vec<u8> {
        let mut out = vec![0u8, 0, 0, 0];
        out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
        for (duration, media_time) in entries {
            out.extend_from_slice(&duration.to_be_bytes());
            out.extend_from_slice(&media_time.to_be_bytes());
            out.extend_from_slice(&[0, 1, 0, 0]);
        }
        out
    }

    #[test]
    fn the_edit_list_rule_is_the_one_ffprobe_follows() {
        // One edit whose media time is two frames of a 15360 timescale:
        // ffmpeg's own B-frame output, which starts at zero.
        let one = edit_of(&elst_v0(&[(30720, 1024)]), 1000, 15360).expect("an edit");
        assert_eq!(
            one,
            Edit {
                empty: 0,
                media_time: Some(1024)
            }
        );
        // An empty edit of a second in front of it: `-output_ts_offset`.
        let two = edit_of(&elst_v0(&[(1000, -1), (30720, 1024)]), 1000, 15360).expect("an edit");
        assert_eq!(
            two,
            Edit {
                empty: 15360,
                media_time: Some(1024)
            }
        );
        // No edit list at all leaves composition times alone, which is
        // what a fragmented file with an empty moov has.
        assert_eq!(
            edit_of(&elst_v0(&[]), 1000, 15360).expect("an edit"),
            Edit::default()
        );
    }

    #[test]
    fn the_zero_snaps_to_the_first_sample_the_edit_keeps() {
        // Composition times a tick short of the nominal frame duration,
        // which is what raw H.264 remuxed into MP4 has, and a media
        // time that therefore lands between two samples.
        let mut track = track_with(&[0i64, 40000, 79999, 119999, 159999]);
        track.edit = Edit {
            empty: 0,
            media_time: Some(96000),
        };
        apply_edit(&mut track);
        assert_eq!(
            track.start_shift, -119999,
            "ffmpeg keeps the first sample at or past the media time"
        );
        assert_eq!(
            track.samples[0].pts, -119999,
            "and ffprobe prints -0.099999"
        );
        assert_eq!(track.samples[3].pts, 0, "the sample it kept is the zero");
    }

    fn track_with(times: &[i64]) -> Track {
        Track::from_parts(
            Handler::Video,
            1_200_000,
            SampleEntry::default(),
            times
                .iter()
                .map(|pts| Sample {
                    pts: *pts,
                    dts: *pts,
                    ..Sample::default()
                })
                .collect(),
        )
    }

    #[test]
    fn a_track_from_parts_numbers_its_own_samples_and_says_nothing_it_was_not_told() {
        let track = Track::from_parts(
            Handler::Video,
            48_000,
            SampleEntry {
                kind: *b"avc1",
                config: vec![1, 2, 3],
                ..SampleEntry::default()
            },
            vec![
                Sample {
                    index: 77,
                    offset: 900,
                    size: 10,
                    dts: 0,
                    pts: 0,
                    duration: 480,
                    keyframe: true,
                },
                Sample {
                    index: 77,
                    offset: 910,
                    size: 12,
                    dts: 480,
                    pts: 480,
                    duration: 480,
                    keyframe: false,
                },
            ],
        );
        // The caller does not have to number the samples.
        assert_eq!(
            track
                .samples
                .iter()
                .map(|one| one.index)
                .collect::<Vec<_>>(),
            [0, 1]
        );
        // And everything the caller did not say is the "nothing was
        // said" value, not a guess.
        assert_eq!(track.track_id, 1);
        assert_eq!(track.movie_timescale, crate::mux::DEFAULT_MOVIE_TIMESCALE);
        assert_eq!(track.edit, Edit::default());
        assert_eq!(track.defaults, TrexDefaults::default());
        assert_eq!(track.start_shift, 0);
        assert!(track.is_video());
        // The decode time after the last sample is derived, not stored.
        assert_eq!(track.next_dts(), 960);
        assert_eq!(
            Track::from_parts(Handler::Audio, 48_000, SampleEntry::default(), Vec::new())
                .next_dts(),
            0
        );
        // A timescale of zero is taken as one rather than divided by.
        let zero = Track::from_parts(Handler::Video, 0, SampleEntry::default(), Vec::new());
        assert_eq!(zero.timescale, 1);
        assert_eq!(zero.ms(2), 2000);
        // The two builders are the only other things a foreign reader
        // needs to say.
        let set = track.clone().with_track_id(7).with_start_shift(-15);
        assert_eq!((set.track_id, set.start_shift), (7, -15));
        assert_eq!(set.samples, track.samples, "and nothing else moved");
    }

    #[test]
    fn a_fragment_read_against_a_track_from_parts_matches_nothing_and_says_so() {
        // `fragment_samples` matches a `traf` against this track's
        // `track_id`, which from parts is 1 unless the caller said
        // otherwise. A foreign reader will not be handing this ISO
        // fragments, and the answer is an empty list rather than a
        // refusal.
        let track = Track::from_parts(Handler::Video, 90_000, SampleEntry::default(), Vec::new());
        let mut traf = full_boxed(b"tfhd", 0, 0x02_0000, &9u32.to_be_bytes());
        traf.extend_from_slice(&full_boxed(b"trun", 0, 0x200, &{
            let mut payload = 1u32.to_be_bytes().to_vec();
            payload.extend_from_slice(&4u32.to_be_bytes());
            payload
        }));
        let moof = boxed(
            b"moof",
            &[
                full_boxed(b"mfhd", 0, 0, &1u32.to_be_bytes()),
                boxed(b"traf", &traf),
            ]
            .concat(),
        );
        assert!(track.fragment_samples(&moof).expect("a read").is_empty());
        // Named as track 9, the same fragment reads.
        let named = track.with_track_id(9);
        assert_eq!(named.fragment_samples(&moof).expect("a read").len(), 1);
    }

    #[test]
    fn a_timescale_of_zero_is_refused_in_a_track_and_replaced_in_a_movie() {
        let mut mdhd = vec![0u8; 12];
        mdhd.extend_from_slice(&0u32.to_be_bytes());
        assert!(mdhd_timescale(&mdhd).is_err());
        let mut mvhd = vec![0u8; 12];
        mvhd.extend_from_slice(&0u32.to_be_bytes());
        assert_eq!(mvhd_timescale(&mvhd).expect("a default"), 1000);
    }

    #[test]
    fn a_version_zero_composition_offset_is_read_as_signed() {
        // ffmpeg writes a negative offset into a version 0 ctts as the
        // two's complement bits, which read unsigned is two billion.
        let mut body = vec![0u8, 0, 0, 0];
        body.extend_from_slice(&1u32.to_be_bytes());
        body.extend_from_slice(&1u32.to_be_bytes());
        body.extend_from_slice(&(-512i32).to_be_bytes());
        assert_eq!(ctts(&body).expect("the offsets"), vec![(1, -512)]);
    }

    #[test]
    fn a_sample_entry_hands_back_its_configuration_record_whole() {
        let avcc = boxed(b"avcC", &[1, 0x64, 0, 0x1f, 0xff]);
        let mut entry = vec![0u8; VISUAL_SAMPLE_ENTRY_LEN - 8];
        entry[24] = 0x01; // width 320
        entry[25] = 0x40;
        entry[26] = 0x00; // height 180
        entry[27] = 0xb4;
        entry.extend_from_slice(&avcc);
        let stsd = {
            let mut body = 1u32.to_be_bytes().to_vec();
            body.extend_from_slice(&boxed(b"avc1", &entry));
            full_boxed(b"stsd", 0, 0, &body)
        };
        let inner = &stsd[8..];
        let read = sample_entry(inner, &Limits::default()).expect("an entry");
        assert_eq!(read.kind, *b"avc1");
        assert_eq!(read.config, &[1, 0x64, 0, 0x1f, 0xff]);
        assert_eq!(
            read.visual,
            Some(Visual {
                width: 320,
                height: 180
            })
        );
    }

    #[test]
    fn a_sample_entry_this_crate_does_not_know_still_names_itself() {
        let entry = boxed(b"vp09", &[0u8; 78]);
        let mut body = 1u32.to_be_bytes().to_vec();
        body.extend_from_slice(&entry);
        let read = sample_entry(&full_boxed(b"stsd", 0, 0, &body)[8..], &Limits::default())
            .expect("an entry");
        assert_eq!(read.kind, *b"vp09");
        assert!(read.config.is_empty());
    }
}

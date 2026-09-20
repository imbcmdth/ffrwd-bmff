//! Building a fragmented file from coded packets: the inverse of
//! [`crate::scanner`] and [`crate::fragment`].
//!
//! One [`Muxer`] serves one coded track. Its
//! [`init_segment`](Muxer::init_segment) is the `ftyp`+`moov` a
//! decoder reads before any sample (§4.3, §8.2.1, §8.8.1), and each
//! [`push`](Muxer::push) turns settled packets into `moof`+`mdat`
//! fragments (§8.8.4, §8.1.1).
//!
//! # The mode, which is not an assumption
//!
//! [`Mode::PerSample`] emits one fragment per sample. That is a
//! publishing convention, not a property of the format: it is what a
//! transport wanting each frame to leave as it is encoded asks for,
//! and it is the default because that is the commoner caller.
//! [`Mode::PerCut`] holds settled samples until
//! [`cut`](Muxer::cut) says to close one fragment over all of them,
//! which is the shape ffmpeg writes, a whole group of pictures to a
//! `moof`. The writer does not care which; nothing in the types
//! assumes one sample.
//!
//! # What it does not write
//!
//! No `elst` (§8.6.6). A file this crate wrote therefore has no edit
//! list, so a reader of it, this crate's own included, sees the
//! composition times exactly as stored: the first sample of a
//! reordering stream is presented at its composition offset rather
//! than at zero. That is what ffprobe reports for ffmpeg's own
//! `frag_keyframe+empty_moov` output too, and it is the honest answer
//! for a stream that has no beginning to trim to.
//!
//! No `mfra` (§8.8.9) and no `sidx`. A random access index of a live
//! stream is a thing a reader of the finished file can build by
//! walking it, which [`crate::track::read`] does.
//!
//! # Timestamps
//!
//! The media timescale is the time base's denominator, so a tick value
//! crosses unchanged when the numerator is 1 and is scaled by it
//! otherwise. A stream that reorders frames may open with negative
//! decode times; the whole decode timeline is shifted up once so the
//! first fragment starts at zero, and presentation offsets carry the
//! reordering per sample (§8.8.8). The first packets of such a stream
//! may arrive with no decode time at all, and are held until one
//! settles, when the prefix gets decode times synthesized backwards at
//! the settled step and flushes.
//!
//! # What goes in
//!
//! A packet's bytes are stored **exactly as given**. This crate knows
//! nothing about NAL units, so an h264 or HEVC caller reframes
//! Annex-B into the length-prefixed form its own `avcC` or `hvcC`
//! declares before pushing, and builds that record itself; `ffrwd-nal`
//! does both. The record is carried into the sample entry as opaque
//! bytes.

use crate::boxes::{boxed, full_boxed};
use crate::track::SampleEntry;
use crate::{Error, Result};

/// The default movie timescale a written `mvhd` declares (§8.2.2).
pub const DEFAULT_MOVIE_TIMESCALE: u32 = 1000;

/// One encoded packet, as a caller hands it over.
pub struct Packet<'a> {
    /// Presentation timestamp in time-base ticks.
    pub pts: i64,
    /// Decode timestamp; absent for the first packets of a reordering
    /// stream whose wire has not settled one.
    pub dts: Option<i64>,
    /// How long the packet is presented, in ticks, where the caller
    /// knows. A packet with one needs no lookahead: its fragment can
    /// leave before the next packet arrives.
    pub duration: Option<i64>,
    /// Whether decoding can start at this packet.
    pub keyframe: bool,
    /// The encoded bytes, in the framing the sample entry declares.
    pub data: &'a [u8],
}

/// One `moof`+`mdat` fragment, ready for a file or a wire.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fragment {
    /// The fragment bytes.
    pub bytes: Vec<u8>,
    /// The `mfhd` sequence number, from 1 (§8.8.5).
    pub sequence: u32,
    /// The `tfdt` base media decode time, in media ticks (§8.8.12).
    pub base_decode_time: u64,
    /// Whether the first sample is a sync sample.
    pub keyframe: bool,
    /// The first sample's presentation timestamp, in the ticks fed in.
    pub pts: i64,
    /// How many samples the fragment holds.
    pub samples: usize,
}

/// When a fragment closes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// One fragment per sample.
    #[default]
    PerSample,
    /// Samples accumulate until [`Muxer::cut`] or
    /// [`Muxer::finish`] closes a fragment over them.
    PerCut,
}

/// A video track to write: the sample entry's four characters, the
/// frame size, and the decoder configuration record that goes inside
/// it (ISO/IEC 14496-15 §5.3.3 for `avcC`, §8.3.3 for `hvcC`, the AV1
/// binding for `av1C`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Video {
    /// `avc1`, `avc3`, `hvc1`, `hev1` or `av01`.
    pub kind: [u8; 4],
    pub width: u32,
    pub height: u32,
    /// The record, opaque: this crate carries it and does not read it.
    pub config: Vec<u8>,
}

/// An AAC track to write: an `mp4a` entry whose `esds` carries the
/// stream's AudioSpecificConfig (ISO/IEC 14496-14 §5.6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Audio {
    pub sample_rate: u32,
    pub channels: u32,
    /// The AudioSpecificConfig, opaque.
    pub config: Vec<u8>,
}

#[derive(Debug)]
enum Kind {
    Video(Video),
    Audio(Audio),
}

#[derive(Debug)]
struct Held {
    data: Vec<u8>,
    pts: i64,
    dts: Option<i64>,
    duration: Option<i64>,
    keyframe: bool,
}

/// One settled sample, waiting for the fragment it belongs to.
#[derive(Debug)]
struct Settled {
    data: Vec<u8>,
    pts: i64,
    base_decode_time: u64,
    entry: TrunEntry,
}

/// Builds an init segment once and fragments as packets arrive.
#[derive(Debug)]
pub struct Muxer {
    timescale: u32,
    movie_timescale: u32,
    /// Ticks multiply by this on the way to media time: the time
    /// base's numerator.
    tick_scale: i64,
    kind: Kind,
    mode: Mode,
    track_id: u32,
    sequence: u32,
    /// Packets whose duration the next decode time has yet to settle.
    pending: Vec<Held>,
    /// Settled samples not yet closed into a fragment, for
    /// [`Mode::PerCut`].
    open: Vec<Settled>,
    /// Added to every decode time; set when the first fragment closes.
    dts_shift: Option<i64>,
    /// The last computed sample duration, for the stream's final
    /// sample.
    last_duration: Option<i64>,
}

impl Muxer {
    /// A muxer for one coded video track.
    pub fn video(video: Video, time_base_num: i32, time_base_den: i32) -> Result<Self> {
        match SampleEntry::config_box(&video.kind) {
            Some(b"esds") | None => {
                return Err(Error::unsupported(
                    "a video sample entry this crate has no carriage for",
                )
                .in_box(video.kind))
            }
            Some(_) => {}
        }
        if video.config.is_empty() {
            return Err(Error::format(
                "a video track with no decoder configuration record to put in its sample entry",
            ));
        }
        Self::build(Kind::Video(video), time_base_num, time_base_den)
    }

    /// A muxer for one coded AAC track.
    pub fn audio(audio: Audio, time_base_num: i32, time_base_den: i32) -> Result<Self> {
        if audio.config.is_empty() {
            return Err(Error::format(
                "an audio track with no AudioSpecificConfig, so no esds can be built",
            ));
        }
        if audio.channels == 0 {
            return Err(Error::format("an audio track that declares no channels"));
        }
        Self::build(Kind::Audio(audio), time_base_num, time_base_den)
    }

    fn build(kind: Kind, time_base_num: i32, time_base_den: i32) -> Result<Self> {
        if time_base_num <= 0 || time_base_den <= 0 {
            return Err(Error::format("a time base that is not positive"));
        }
        Ok(Self {
            timescale: time_base_den as u32,
            movie_timescale: DEFAULT_MOVIE_TIMESCALE,
            tick_scale: i64::from(time_base_num),
            kind,
            mode: Mode::PerSample,
            track_id: 1,
            sequence: 1,
            pending: Vec::new(),
            open: Vec::new(),
            dts_shift: None,
            last_duration: None,
        })
    }

    /// When fragments close. The default is [`Mode::PerSample`].
    pub fn with_mode(mut self, mode: Mode) -> Self {
        self.mode = mode;
        self
    }

    /// The movie timescale the `mvhd` declares (§8.2.2). The default is
    /// [`DEFAULT_MOVIE_TIMESCALE`]. Nothing this crate writes is
    /// counted in it, because it writes no edit list and no durations
    /// in the `moov`, but a reader that prints it should be told the
    /// truth.
    pub fn with_movie_timescale(mut self, timescale: u32) -> Self {
        self.movie_timescale = timescale.max(1);
        self
    }

    /// The `track_ID` every box that names one will carry (§8.3.2).
    pub fn with_track_id(mut self, track_id: u32) -> Self {
        self.track_id = track_id.max(1);
        self
    }

    /// The media timescale, which is the time base's denominator.
    pub fn timescale(&self) -> u32 {
        self.timescale
    }

    /// The decoder configuration record this muxer writes into its
    /// sample entry, as it was given.
    pub fn config(&self) -> &[u8] {
        match &self.kind {
            Kind::Video(video) => &video.config,
            Kind::Audio(audio) => &audio.config,
        }
    }

    /// The `ftyp`+`moov` a decoder reads before any fragment.
    pub fn init_segment(&self) -> Vec<u8> {
        let mut out = self.ftyp();
        out.extend_from_slice(&self.moov());
        out
    }

    /// Collects one packet and returns every fragment it settles.
    ///
    /// In [`Mode::PerSample`] that is this packet, when it carries its
    /// own duration; else the packet before it, whose duration its
    /// decode time is; or several, where its decode time settles a
    /// held prefix; or none, while the stream's reorder delay keeps
    /// decode times unsettled. In [`Mode::PerCut`] it is never any
    /// until [`cut`](Self::cut) is called.
    pub fn push(&mut self, packet: Packet<'_>) -> Result<Vec<Fragment>> {
        if packet.data.is_empty() {
            return Err(Error::format("a packet with no bytes"));
        }
        self.pending.push(Held {
            data: packet.data.to_vec(),
            pts: packet.pts,
            dts: packet.dts,
            duration: packet.duration.filter(|value| *value > 0),
            keyframe: packet.keyframe,
        });
        self.drain(false)
    }

    /// Closes a fragment over every settled sample held so far, for
    /// [`Mode::PerCut`]. `None` when nothing is held.
    ///
    /// In [`Mode::PerSample`] nothing is ever held, so this is always
    /// `None`.
    pub fn cut(&mut self) -> Result<Option<Fragment>> {
        Ok(self.close())
    }

    /// Flushes everything: a duration-less last sample, whose duration
    /// no following decode time will settle, and any prefix a reorder
    /// delay kept unsettled to the end.
    pub fn finish(&mut self) -> Result<Vec<Fragment>> {
        let mut out = self.drain(true)?;
        out.extend(self.close());
        Ok(out)
    }

    /// Emits every pending sample whose decode time and duration are
    /// settled.
    ///
    /// A sample's duration is the caller's own where the packet
    /// carried one; only a duration-less tail waits for the next
    /// decode time to settle it as a step. A flush emits everything,
    /// such a tail at the last known duration.
    fn drain(&mut self, flush: bool) -> Result<Vec<Fragment>> {
        let dts = self.resolved_dts(flush)?;
        // `dts` covers no pending sample or every one of them, so only
        // the final sample can lack a settled duration.
        let holds_tail =
            !flush && !dts.is_empty() && self.pending[dts.len() - 1].duration.is_none();
        let emit = dts.len() - usize::from(holds_tail);
        if emit == 0 {
            return Ok(Vec::new());
        }
        let samples: Vec<Held> = self.pending.drain(..emit).collect();
        let mut out = Vec::new();
        for (index, sample) in samples.into_iter().enumerate() {
            let duration = match (sample.duration, dts.get(index + 1)) {
                (Some(own), _) => own,
                (None, Some(next)) => {
                    let step = next - dts[index];
                    if step < 0 {
                        return Err(Error::format("a decode time that steps backwards"));
                    }
                    step
                }
                (None, None) => self.last_duration.unwrap_or(0),
            };
            if duration > 0 {
                self.last_duration = Some(duration);
            }
            self.settle(sample, dts[index], duration)?;
            if self.mode == Mode::PerSample {
                out.extend(self.close());
            }
        }
        Ok(out)
    }

    /// One decode time per pending sample, for as many as are settled
    /// or synthesizable now: settled ones as they came, an unsettled
    /// prefix synthesized backwards from the first settled pair at its
    /// step. Empty while nothing has settled, or while a prefix waits
    /// for the pair; a flush resolves everything, presentation order
    /// standing in for a stream that never settled a decode time.
    fn resolved_dts(&self, flush: bool) -> Result<Vec<i64>> {
        let samples = &self.pending;
        if samples.is_empty() {
            return Ok(Vec::new());
        }
        let Some(k) = samples.iter().position(|held| held.dts.is_some()) else {
            // No decode time settled anywhere: at end of input this is
            // a stream that does not reorder, where presentation order
            // is decode order.
            return if flush {
                Ok(samples.iter().map(|held| held.pts).collect())
            } else {
                Ok(Vec::new())
            };
        };
        let known = samples[k].dts.expect("position found it");
        let step = samples
            .get(k + 1)
            .and_then(|held| held.dts)
            .map(|next| next - known);
        let step = match step {
            Some(step) => step,
            // No prefix to synthesize, so no step is needed.
            None if k == 0 => 0,
            None if flush => 0,
            // Hold the prefix until a second decode time sets the step.
            None => return Ok(Vec::new()),
        };
        let mut out = Vec::with_capacity(samples.len());
        for (index, sample) in samples.iter().enumerate() {
            match sample.dts {
                Some(dts) => out.push(dts),
                None if index < k => out.push(known - step * (k - index) as i64),
                None => {
                    return Err(Error::format(
                        "a packet with no decode time after one settled",
                    ))
                }
            }
        }
        Ok(out)
    }

    /// One settled sample into the fragment being built.
    fn settle(&mut self, sample: Held, dts: i64, duration: i64) -> Result<()> {
        let shift = *self.dts_shift.get_or_insert(if dts < 0 { -dts } else { 0 });
        let base_decode_time = dts + shift;
        if base_decode_time < 0 {
            return Err(Error::format("a decode time below the stream's start"));
        }

        let cts = (sample.pts - dts) * self.tick_scale;
        let cts: i32 = cts
            .try_into()
            .map_err(|_| Error::format("a presentation offset that overflows its field"))?;
        let duration = duration * self.tick_scale;
        let duration: u32 = duration
            .try_into()
            .map_err(|_| Error::format("a duration that overflows its field"))?;
        // §8.8.3's sample flags: `sample_depends_on` 2 (an I picture)
        // on a sync sample, 1 with `sample_is_non_sync_sample` set on
        // anything else.
        let flags: u32 = if sample.keyframe {
            0x0200_0000
        } else {
            0x0101_0000
        };
        self.open.push(Settled {
            pts: sample.pts,
            base_decode_time: (base_decode_time * self.tick_scale) as u64,
            entry: TrunEntry {
                duration,
                size: sample.data.len() as u32,
                flags,
                cts,
            },
            data: sample.data,
        });
        Ok(())
    }

    /// Closes a fragment over whatever is open.
    fn close(&mut self) -> Option<Fragment> {
        if self.open.is_empty() {
            return None;
        }
        let held: Vec<Settled> = std::mem::take(&mut self.open);
        let base_decode_time = held[0].base_decode_time;
        let pts = held[0].pts;
        let keyframe = held[0].entry.flags & crate::fragment::NON_SYNC == 0;
        let samples = held.len();
        let entries: Vec<TrunEntry> = held.iter().map(|one| one.entry).collect();
        let payload: usize = held.iter().map(|one| one.data.len()).sum();

        let mut bytes = moof(self.track_id, self.sequence, base_decode_time, &entries);
        bytes.extend_from_slice(&((payload + 8) as u32).to_be_bytes());
        bytes.extend_from_slice(b"mdat");
        for one in &held {
            bytes.extend_from_slice(&one.data);
        }

        let fragment = Fragment {
            bytes,
            sequence: self.sequence,
            base_decode_time,
            keyframe,
            pts,
            samples,
        };
        self.sequence += 1;
        Some(fragment)
    }

    fn ftyp(&self) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(b"iso5"); // major brand
        payload.extend_from_slice(&512u32.to_be_bytes()); // minor version
        payload.extend_from_slice(b"iso5");
        payload.extend_from_slice(b"isom");
        payload.extend_from_slice(&self.entry_kind());
        payload.extend_from_slice(b"mp41");
        boxed(b"ftyp", &payload)
    }

    fn entry_kind(&self) -> [u8; 4] {
        match &self.kind {
            Kind::Video(video) => video.kind,
            Kind::Audio(_) => *b"mp4a",
        }
    }

    fn moov(&self) -> Vec<u8> {
        let (entry, handler, handler_name, media_header) = match &self.kind {
            Kind::Video(video) => (
                visual_entry(video),
                b"vide",
                &b"VideoHandler\0"[..],
                full_boxed(b"vmhd", 0, 1, &[0; 8]),
            ),
            Kind::Audio(audio) => (
                audio_entry(audio),
                b"soun",
                &b"SoundHandler\0"[..],
                // balance and reserved, both zero.
                full_boxed(b"smhd", 0, 0, &[0; 4]),
            ),
        };
        let stbl = boxed(
            b"stbl",
            &[
                full_boxed(b"stsd", 0, 0, &{
                    let mut payload = 1u32.to_be_bytes().to_vec(); // entry_count
                    payload.extend_from_slice(&entry);
                    payload
                }),
                full_boxed(b"stts", 0, 0, &[0; 4]),
                full_boxed(b"stsc", 0, 0, &[0; 4]),
                full_boxed(b"stsz", 0, 0, &[0; 8]),
                full_boxed(b"stco", 0, 0, &[0; 4]),
            ]
            .concat(),
        );
        let dinf = boxed(
            b"dinf",
            &full_boxed(b"dref", 0, 0, &{
                let mut payload = vec![0, 0, 0, 1]; // entry_count
                payload.extend_from_slice(&full_boxed(b"url ", 0, 1, &[]));
                payload
            }),
        );
        let minf = boxed(b"minf", &[media_header, dinf, stbl].concat());
        let hdlr = full_boxed(b"hdlr", 0, 0, &{
            let mut payload = vec![0; 4]; // pre_defined
            payload.extend_from_slice(handler);
            payload.extend_from_slice(&[0; 12]);
            payload.extend_from_slice(handler_name);
            payload
        });
        let mdhd = full_boxed(b"mdhd", 0, 0, &{
            let mut payload = vec![0; 8]; // creation, modification
            payload.extend_from_slice(&self.timescale.to_be_bytes());
            payload.extend_from_slice(&[0; 4]); // duration: told by fragments
            payload.extend_from_slice(&0x55c4u16.to_be_bytes()); // und
            payload.extend_from_slice(&[0; 2]);
            payload
        });
        let mdia = boxed(b"mdia", &[mdhd, hdlr, minf].concat());
        // A video track states its frame size and no volume; an audio
        // track states full volume and no frame size.
        let (volume, width, height) = match &self.kind {
            Kind::Video(video) => (0u16, video.width, video.height),
            Kind::Audio(_) => (0x0100, 0, 0),
        };
        let tkhd = full_boxed(b"tkhd", 0, 3, &{
            let mut payload = vec![0; 8]; // creation, modification
            payload.extend_from_slice(&self.track_id.to_be_bytes());
            payload.extend_from_slice(&[0; 4]); // reserved
            payload.extend_from_slice(&[0; 4]); // duration: told by fragments
            payload.extend_from_slice(&[0; 8]); // reserved
            payload.extend_from_slice(&[0; 4]); // layer, alternate group
            payload.extend_from_slice(&volume.to_be_bytes());
            payload.extend_from_slice(&[0; 2]); // reserved
            payload.extend_from_slice(&MATRIX);
            payload.extend_from_slice(&(width << 16).to_be_bytes());
            payload.extend_from_slice(&(height << 16).to_be_bytes());
            payload
        });
        let trak = boxed(b"trak", &[tkhd, mdia].concat());
        let mvex = boxed(
            b"mvex",
            &full_boxed(b"trex", 0, 0, &{
                let mut payload = self.track_id.to_be_bytes().to_vec();
                payload.extend_from_slice(&1u32.to_be_bytes()); // sample description
                payload.extend_from_slice(&[0; 12]); // default duration, size, flags
                payload
            }),
        );
        let mvhd = full_boxed(b"mvhd", 0, 0, &{
            let mut payload = vec![0; 8]; // creation, modification
            payload.extend_from_slice(&self.movie_timescale.to_be_bytes());
            payload.extend_from_slice(&[0; 4]); // duration: told by fragments
            payload.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate 1.0
            payload.extend_from_slice(&0x0100u16.to_be_bytes()); // volume 1.0
            payload.extend_from_slice(&[0; 10]); // reserved
            payload.extend_from_slice(&MATRIX);
            payload.extend_from_slice(&[0; 24]); // pre_defined
            payload.extend_from_slice(&self.track_id.saturating_add(1).to_be_bytes());
            payload
        });
        boxed(b"moov", &[mvhd, trak, mvex].concat())
    }
}

/// The identity transformation matrix every `mvhd` and `tkhd` carries
/// (§8.2.2, §8.3.2): 16.16 fixed point but for the last column, which
/// is 2.30.
const MATRIX: [u8; 36] = {
    let mut m = [0u8; 36];
    m[1] = 0x01; // 0x00010000
    m[17] = 0x01;
    m[32] = 0x40; // 0x40000000
    m
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TrunEntry {
    duration: u32,
    size: u32,
    flags: u32,
    cts: i32,
}

/// One `moof` over `entries`, whose `trun` data offset points past the
/// `moof` and the `mdat` header at the payload (§8.8.8).
fn moof(track_id: u32, sequence: u32, base_decode_time: u64, entries: &[TrunEntry]) -> Vec<u8> {
    // Sizes first, so the trun's data offset can point past the moof
    // into the mdat payload before either is assembled.
    let trun_size = 20 + 16 * entries.len();
    let traf_size = 8 + 16 + 20 + trun_size;
    let moof_size = 8 + 16 + traf_size;
    let data_offset = (moof_size + 8) as i32;

    let mfhd = full_boxed(b"mfhd", 0, 0, &sequence.to_be_bytes());
    // `default-base-is-moof`: every offset in this fragment is counted
    // from the moof's own first byte (§8.8.7).
    let tfhd = full_boxed(b"tfhd", 0, 0x02_0000, &track_id.to_be_bytes());
    let tfdt = full_boxed(b"tfdt", 1, 0, &base_decode_time.to_be_bytes());
    // Flags: data-offset, sample-duration, sample-size, sample-flags,
    // sample-composition-time-offset. Version 1, whose offset is
    // signed.
    let trun = full_boxed(b"trun", 1, 0xf01, &{
        let mut payload = (entries.len() as u32).to_be_bytes().to_vec();
        payload.extend_from_slice(&data_offset.to_be_bytes());
        for entry in entries {
            payload.extend_from_slice(&entry.duration.to_be_bytes());
            payload.extend_from_slice(&entry.size.to_be_bytes());
            payload.extend_from_slice(&entry.flags.to_be_bytes());
            payload.extend_from_slice(&entry.cts.to_be_bytes());
        }
        payload
    });
    let traf = boxed(b"traf", &[tfhd, tfdt, trun].concat());
    let built = boxed(b"moof", &[mfhd, traf].concat());
    debug_assert_eq!(built.len(), moof_size);
    built
}

/// A `VisualSampleEntry` with the configuration record inside it
/// (§12.1.3).
fn visual_entry(video: &Video) -> Vec<u8> {
    let config_box = SampleEntry::config_box(&video.kind).expect("a checked entry kind");
    let mut payload = Vec::new();
    payload.extend_from_slice(&[0; 6]); // reserved
    payload.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
    payload.extend_from_slice(&[0; 16]); // pre_defined, reserved
    payload.extend_from_slice(&(video.width as u16).to_be_bytes());
    payload.extend_from_slice(&(video.height as u16).to_be_bytes());
    payload.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // 72 dpi
    payload.extend_from_slice(&0x0048_0000u32.to_be_bytes());
    payload.extend_from_slice(&[0; 4]); // reserved
    payload.extend_from_slice(&1u16.to_be_bytes()); // frame_count
    payload.extend_from_slice(&[0; 32]); // compressorname, empty
    payload.extend_from_slice(&0x0018u16.to_be_bytes()); // depth
    payload.extend_from_slice(&(-1i16).to_be_bytes()); // pre_defined
    payload.extend_from_slice(&boxed(config_box, &video.config));
    boxed(&video.kind, &payload)
}

/// An `mp4a` `AudioSampleEntry` with its `esds` inside (§12.2.3,
/// ISO/IEC 14496-14 §5.6).
fn audio_entry(audio: &Audio) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&[0; 6]); // reserved
    payload.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
    payload.extend_from_slice(&[0; 8]); // version, revision, vendor
    payload.extend_from_slice(&(audio.channels.min(0xffff) as u16).to_be_bytes());
    payload.extend_from_slice(&16u16.to_be_bytes()); // samplesize
    payload.extend_from_slice(&[0; 4]); // pre_defined, reserved
                                        // 16.16 fixed point, so a rate past 65535 does not fit and is left
                                        // at zero; the esds below carries the real one either way.
    let rate = if audio.sample_rate > u32::from(u16::MAX) {
        0
    } else {
        audio.sample_rate << 16
    };
    payload.extend_from_slice(&rate.to_be_bytes());
    payload.extend_from_slice(&full_boxed(b"esds", 0, 0, &esds(&audio.config)));
    boxed(b"mp4a", &payload)
}

/// The `esds` payload: the ES descriptor holding a decoder config
/// whose specific info is the caller's own AudioSpecificConfig
/// (ISO/IEC 14496-1 §7.2.6).
pub fn esds(asc: &[u8]) -> Vec<u8> {
    let specific = descriptor(0x05, asc);
    let config = descriptor(0x04, &{
        let mut payload = vec![
            0x40, // MPEG-4 audio
            0x15, // audio stream, not upstream
            0, 0, 0, // buffer size
        ];
        payload.extend_from_slice(&0u32.to_be_bytes()); // max bitrate: unstated
        payload.extend_from_slice(&0u32.to_be_bytes()); // average bitrate: unstated
        payload.extend_from_slice(&specific);
        payload
    });
    // SLConfigDescriptor, predefined 2: the timing an mp4 track carries.
    let sl = descriptor(0x06, &[0x02]);
    descriptor(0x03, &{
        let mut payload = 0u16.to_be_bytes().to_vec(); // ES_ID
        payload.push(0); // stream priority, no dependency, no URL
        payload.extend_from_slice(&config);
        payload.extend_from_slice(&sl);
        payload
    })
}

/// One MPEG-4 descriptor: a tag, its length in the seven-bits-a-byte
/// coding descriptors use, then the payload.
fn descriptor(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let mut length = payload.len();
    let mut coded = vec![(length & 0x7f) as u8];
    length >>= 7;
    while length > 0 {
        coded.push((length & 0x7f) as u8 | 0x80);
        length >>= 7;
    }
    coded.reverse();
    out.extend_from_slice(&coded);
    out.extend_from_slice(payload);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::boxes::children;
    use crate::track::Track;

    /// A plausible `avcC` payload. Nothing here reads it.
    const AVCC: &[u8] = &[
        1, 66, 0xc0, 30, 0xff, 0xe1, 0, 2, 0x67, 66, 1, 0, 2, 0x68, 0xee,
    ];

    /// The 5-byte AudioSpecificConfig ffmpeg writes for 48 kHz mono
    /// AAC-LC.
    const ASC: &[u8] = &[0x11, 0x88, 0x56, 0xe5, 0x00];

    fn muxer() -> Muxer {
        Muxer::video(
            Video {
                kind: *b"avc1",
                width: 320,
                height: 180,
                config: AVCC.to_vec(),
            },
            1,
            30,
        )
        .expect("a muxer")
    }

    fn audio_muxer() -> Muxer {
        Muxer::audio(
            Audio {
                sample_rate: 48000,
                channels: 1,
                config: ASC.to_vec(),
            },
            1,
            48000,
        )
        .expect("a muxer")
    }

    fn sample(pts: i64) -> Vec<u8> {
        vec![0, 0, 0, 2, 0x65, pts as u8]
    }

    #[test]
    fn each_push_settles_the_sample_before_it() {
        let mut mux = muxer();
        let mut fragments = Vec::new();
        for pts in 0..4i64 {
            let keyframe = pts % 3 == 0;
            let data = sample(pts);
            let emitted = mux
                .push(Packet {
                    pts,
                    dts: Some(pts),
                    duration: None,
                    keyframe,
                    data: &data,
                })
                .expect("push");
            // The first push has nothing settled; each later one
            // settles exactly the packet before it.
            assert_eq!(emitted.len(), usize::from(pts > 0));
            fragments.extend(emitted);
        }
        fragments.extend(mux.finish().expect("finish"));
        assert!(mux.finish().expect("finish").is_empty());

        assert_eq!(fragments.len(), 4, "one fragment per sample");
        for (index, fragment) in fragments.iter().enumerate() {
            assert_eq!(fragment.sequence, index as u32 + 1);
            assert_eq!(fragment.pts, index as i64);
            assert_eq!(fragment.samples, 1);
        }
        let syncs: Vec<bool> = fragments.iter().map(|one| one.keyframe).collect();
        assert_eq!(syncs, [true, false, false, true]);
    }

    #[test]
    fn a_packet_with_its_own_duration_leaves_on_its_own_push() {
        let mut mux = muxer();
        for pts in 0..3i64 {
            let data = sample(pts);
            let emitted = mux
                .push(Packet {
                    pts,
                    dts: Some(pts),
                    duration: Some(1),
                    keyframe: pts == 0,
                    data: &data,
                })
                .expect("push");
            assert_eq!(emitted.len(), 1, "pts {pts}: no lookahead was needed");
            assert_eq!(emitted[0].pts, pts);
        }
        assert!(mux.finish().expect("finish").is_empty(), "nothing held");
    }

    #[test]
    fn the_final_sample_keeps_its_real_duration() {
        let mut mux = muxer();
        let mut fragments = Vec::new();
        for pts in [0i64, 2] {
            let data = sample(pts);
            fragments.extend(
                mux.push(Packet {
                    pts,
                    dts: Some(pts),
                    duration: None,
                    keyframe: pts == 0,
                    data: &data,
                })
                .expect("push"),
            );
        }
        let data = sample(4);
        fragments.extend(
            mux.push(Packet {
                pts: 4,
                dts: Some(4),
                duration: Some(5),
                keyframe: false,
                data: &data,
            })
            .expect("push"),
        );
        assert_eq!(
            fragments.iter().map(|one| one.pts).collect::<Vec<_>>(),
            [0, 2, 4],
            "the packet that carried its own duration needed no flush"
        );
        let durations: Vec<u32> = fragments
            .iter()
            .map(|one| trun_entry(&one.bytes, 0).0)
            .collect();
        assert_eq!(durations, [2, 2, 5]);
        assert!(mux.finish().expect("finish").is_empty());
    }

    #[test]
    fn an_unsettled_decode_prefix_is_held_then_synthesized_backwards() {
        let mut mux = muxer();
        for (pts, dts) in [(2i64, None), (0, None), (1, Some(0i64))] {
            let data = sample(pts);
            let held = mux
                .push(Packet {
                    pts,
                    dts,
                    duration: None,
                    keyframe: pts == 2,
                    data: &data,
                })
                .expect("push");
            assert!(
                held.is_empty(),
                "nothing may leave before the step is known"
            );
        }
        let data = sample(3);
        let emitted = mux
            .push(Packet {
                pts: 3,
                dts: Some(1),
                duration: None,
                keyframe: false,
                data: &data,
            })
            .expect("push");
        assert_eq!(
            emitted.iter().map(|one| one.pts).collect::<Vec<_>>(),
            [2, 0, 1]
        );
        // Synthesized: dts -2 behind the settled 0, so the shift lifts
        // the first fragment's base decode time to zero exactly.
        assert_eq!(emitted[0].base_decode_time, 0);
        assert_eq!(emitted[1].base_decode_time, 1);
        // The reorder still crosses as a presentation offset.
        assert_eq!(trun_entry(&emitted[0].bytes, 0).3, 4);
        let tail = mux.finish().expect("finish");
        assert_eq!(tail.iter().map(|one| one.pts).collect::<Vec<_>>(), [3]);
    }

    #[test]
    fn a_cut_mode_muxer_puts_every_settled_sample_in_one_fragment() {
        let mut mux = muxer().with_mode(Mode::PerCut);
        for pts in 0..4i64 {
            let data = sample(pts);
            let emitted = mux
                .push(Packet {
                    pts,
                    dts: Some(pts),
                    duration: Some(1),
                    keyframe: pts == 0,
                    data: &data,
                })
                .expect("push");
            assert!(emitted.is_empty(), "nothing leaves until a cut");
        }
        let fragment = mux.cut().expect("a cut").expect("a fragment");
        assert_eq!(fragment.samples, 4);
        assert_eq!(fragment.sequence, 1);
        assert_eq!(trun_sample_count(&fragment.bytes), 4);
        assert!(mux.cut().expect("a cut").is_none(), "nothing left open");
        // And the reader reads the four samples back out of the one
        // fragment, which is the shape ffmpeg writes.
        let track = Track::from_init(&mux.init_segment()).expect("an init segment");
        let read = track
            .fragment_samples(&fragment.bytes)
            .expect("the samples");
        assert_eq!(read.len(), 4);
        assert_eq!(
            read.iter().map(|one| one.dts).collect::<Vec<_>>(),
            [0, 1, 2, 3]
        );
    }

    #[test]
    fn an_empty_packet_is_refused() {
        let mut mux = muxer();
        let err = mux
            .push(Packet {
                pts: 0,
                dts: Some(0),
                duration: None,
                keyframe: true,
                data: &[],
            })
            .expect_err("a refusal");
        assert!(err.to_string().contains("no bytes"), "{err}");
    }

    #[test]
    fn a_track_with_no_configuration_record_has_no_sample_entry_to_build() {
        let err = Muxer::audio(
            Audio {
                sample_rate: 48000,
                channels: 2,
                config: Vec::new(),
            },
            1,
            48000,
        )
        .expect_err("a refusal");
        assert!(err.to_string().contains("AudioSpecificConfig"), "{err}");
        let err = Muxer::video(
            Video {
                kind: *b"vp09",
                width: 16,
                height: 16,
                config: vec![1],
            },
            1,
            30,
        )
        .expect_err("a refusal");
        assert!(err.to_string().contains("vp09"), "{err}");
    }

    #[test]
    fn every_video_entry_this_crate_writes_reads_back_as_itself() {
        // The three the read side knows, written from an opaque record
        // and read back with the record intact.
        for (kind, config_box) in [
            (*b"avc1", *b"avcC"),
            (*b"hvc1", *b"hvcC"),
            (*b"av01", *b"av1C"),
        ] {
            let mux = Muxer::video(
                Video {
                    kind,
                    width: 320,
                    height: 180,
                    config: vec![1, 2, 3, 4],
                },
                1,
                30,
            )
            .expect("a muxer");
            let init = mux.init_segment();
            let track = Track::from_init(&init).expect("an init segment");
            assert_eq!(track.entry.kind, kind);
            assert_eq!(track.entry.config, vec![1, 2, 3, 4]);
            assert_eq!(track.timescale, 30);
            assert!(init.windows(4).any(|four| four == config_box));
        }
    }

    #[test]
    fn the_audio_entry_carries_the_config_inside_its_esds() {
        let mux = audio_muxer();
        let init = mux.init_segment();
        let track = Track::from_init(&init).expect("an init segment");
        assert_eq!(track.entry.kind, *b"mp4a");
        assert!(track.is_audio());
        assert_eq!(
            track.entry.audio.expect("the sound").sample_rate,
            48000,
            "the 16.16 rate's whole part"
        );
        // The descriptor chain ends with the config verbatim, behind
        // its tag and length byte.
        let config = track
            .entry
            .config
            .windows(ASC.len())
            .position(|window| window == ASC)
            .expect("the config is in the esds");
        assert_eq!(
            &track.entry.config[config - 2..config],
            &[0x05, ASC.len() as u8]
        );
        assert!(init.windows(4).any(|four| four == b"soun"));
        assert!(init.windows(4).any(|four| four == b"smhd"));
        assert!(!init.windows(4).any(|four| four == b"vmhd"));
    }

    #[test]
    fn a_long_config_codes_its_descriptor_length_across_bytes() {
        let long = vec![0x11u8; 200];
        let coded = descriptor(0x05, &long);
        assert_eq!(coded[0], 0x05);
        assert_eq!(&coded[1..3], &[0x81, 0x48]);
        assert_eq!(&coded[3..], &long[..]);
    }

    #[test]
    fn the_written_fragment_is_the_one_the_reader_reads() {
        let mut mux = muxer();
        let mut fragments = Vec::new();
        for pts in 0..3i64 {
            let data = sample(pts);
            fragments.extend(
                mux.push(Packet {
                    pts,
                    dts: Some(pts),
                    duration: Some(1),
                    keyframe: pts == 0,
                    data: &data,
                })
                .expect("push"),
            );
        }
        let track = Track::from_init(&mux.init_segment()).expect("an init segment");
        for (index, fragment) in fragments.iter().enumerate() {
            let read = track.fragment_samples(&fragment.bytes).expect("a fragment");
            assert_eq!(read.len(), 1);
            assert_eq!(read[0].dts, index as i64);
            assert_eq!(read[0].keyframe, index == 0);
            let bytes = crate::fragment::sample_bytes(&fragment.bytes, 0, &read[0])
                .expect("the sample bytes");
            assert_eq!(bytes, &sample(index as i64)[..]);
        }
    }

    #[test]
    fn the_init_segment_is_two_top_level_boxes_and_nothing_else() {
        let init = muxer().init_segment();
        let top = children(&init, 0).expect("the boxes");
        assert_eq!(top.len(), 2);
        assert!(top[0].is(b"ftyp"));
        assert!(top[1].is(b"moov"));
        assert!(
            !init.windows(4).any(|four| four == b"elst"),
            "this writer writes no edit list"
        );
    }

    /// The trun's sample count, out of a fragment this module wrote.
    fn trun_sample_count(bytes: &[u8]) -> u32 {
        let at = past_tag(bytes, b"trun") + 4;
        u32::from_be_bytes(bytes[at..at + 4].try_into().expect("count"))
    }

    /// One trun entry: duration, size, flags, cts.
    fn trun_entry(bytes: &[u8], index: usize) -> (u32, u32, u32, i32) {
        let at = past_tag(bytes, b"trun") + 12 + index * 16;
        let field = |offset: usize| {
            u32::from_be_bytes(
                bytes[at + 4 * offset..at + 4 * offset + 4]
                    .try_into()
                    .expect("field"),
            )
        };
        (field(0), field(1), field(2), field(3) as i32)
    }

    fn past_tag(bytes: &[u8], kind: &[u8; 4]) -> usize {
        bytes
            .windows(4)
            .position(|window| window == kind)
            .map(|at| at + 4)
            .expect("box present")
    }
}

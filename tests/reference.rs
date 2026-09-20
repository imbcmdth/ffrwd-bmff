//! The crate pinned against a fragmented file real ffmpeg wrote.
//!
//! `tests/data/ref-frag.mp4` is the only fixture committed here, and it
//! is forty kilobytes. Everything else the tests need is made by ffmpeg
//! when they run; see `tests/ffmpeg.rs`. This one is committed because
//! the claim it supports is "the writer produces what ffmpeg produces",
//! and a claim like that is worth holding still. It was made with:
//!
//!     ffmpeg -f lavfi -i testsrc2=size=160x90:rate=30 -t 2 \
//!       -c:v libx264 -preset veryfast -g 15 -bf 2 -pix_fmt yuv420p \
//!       -f mp4 -movflags frag_keyframe+empty_moov+default_base_moof \
//!       tests/data/ref-frag.mp4
//!
//! Sixty frames, four groups of pictures, B-frames on, so the file has
//! a whole group of pictures to a `moof` and composition offsets that
//! reorder.
//!
//! Nothing asserted here comes from the code under test. The reference
//! is taken apart by this file's own hand-written parser, which reads
//! the `trun` sample table straight out of the bytes, and everything
//! else is compared against that.

use std::process::Command;

use ffrwd_bmff::fragment::sample_bytes;
use ffrwd_bmff::mux::{Fragment, Mode, Muxer, Packet, Video};
use ffrwd_bmff::scanner::{Scanner, Segment};
use ffrwd_bmff::source::Source;
use ffrwd_bmff::track::{self, Pick, Track, Visual};

const REFERENCE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/ref-frag.mp4");
const NON_SYNC: u32 = 0x0001_0000;

fn bytes() -> Vec<u8> {
    std::fs::read(REFERENCE).expect("the committed reference")
}

// ------------------------------------------------------------------ //
// The scanner and the demux.
// ------------------------------------------------------------------ //

#[test]
fn the_scanner_cuts_the_reference_where_ffmpeg_cut_it() {
    let reference = Reference::read();
    assert!(
        reference.fragments.len() >= 3,
        "the fixture is several groups of pictures"
    );
    for fragment in &reference.fragments {
        let first = fragment.samples.first().expect("samples");
        assert!(
            first.flags & NON_SYNC == 0,
            "fragment {} does not open on a sync sample",
            fragment.sequence
        );
    }
    // And the same cuts come out however the bytes arrive.
    let whole = bytes();
    for step in [1usize, 17, 1024, 1 << 20] {
        let mut scanner = Scanner::new();
        let mut fragments = 0usize;
        let mut inits = 0usize;
        for chunk in whole.chunks(step) {
            scanner.push(chunk).expect("a push");
            while let Some(segment) = scanner.poll() {
                match segment {
                    Segment::Init(_) => inits += 1,
                    Segment::Fragment(_) => fragments += 1,
                }
            }
        }
        scanner.finish().expect("a clean end");
        assert_eq!((inits, fragments), (1, reference.fragments.len()), "{step}");
    }
}

#[test]
fn the_demux_reads_the_reference_the_hand_parser_does() {
    // ffmpeg puts a whole group of pictures in one moof, which the
    // per-sample writer never does: the reader has to cut a many-sample
    // trun exactly where this file's own parser cuts it.
    let reference = Reference::read();
    let mut scanner = Scanner::new();
    scanner.push(&bytes()).expect("scan the reference");
    scanner.finish().expect("a clean end");

    let mut track: Option<Track> = None;
    let mut read = Vec::new();
    while let Some(segment) = scanner.poll() {
        match segment {
            Segment::Init(seg) => {
                track = Some(Track::from_init(&seg.bytes).expect("the init segment reads"))
            }
            Segment::Fragment(frag) => {
                let track = track.as_ref().expect("the init segment comes first");
                for sample in track
                    .fragment_samples(&frag.bytes)
                    .expect("a fragment reads")
                {
                    let data = sample_bytes(&frag.bytes, 0, &sample).expect("the bytes");
                    read.push((sample, data.to_vec()));
                }
            }
        }
    }
    let track = track.expect("the reference has an init segment");
    assert_eq!(track.timescale, reference.timescale);
    assert_eq!(track.entry.kind, *b"avc1");
    assert_eq!(track.entry.config, reference.avcc);
    assert_eq!(
        track.entry.visual,
        Some(Visual {
            width: reference.width,
            height: reference.height
        })
    );
    assert_eq!(read.len(), reference.samples.len(), "sample count");
    for (index, ((sample, data), want)) in read.iter().zip(&reference.samples).enumerate() {
        assert_eq!(
            (sample.dts, sample.pts, sample.duration, sample.keyframe),
            (want.dts, want.pts, want.duration as i64, want.keyframe),
            "sample {index}"
        );
        assert_eq!(data, &want.data, "sample {index} bytes");
    }
}

#[test]
fn the_file_reader_and_the_scanner_read_the_same_samples() {
    // The same file through the other face of the crate: a seekable
    // read that walks every moof rather than a stream cut into
    // segments. A fragmented file with an empty moov has no edit list,
    // so nothing shifts.
    let reference = Reference::read();
    let mut src = Source::new(std::io::Cursor::new(bytes())).expect("a source");
    let track = track::read(&mut src, Pick::Video).expect("a track");
    assert_eq!(track.start_shift, 0, "an empty moov carries no elst");
    assert_eq!(track.samples.len(), reference.samples.len());
    for (index, (sample, want)) in track.samples.iter().zip(&reference.samples).enumerate() {
        assert_eq!(
            (sample.dts, sample.pts, sample.size as usize),
            (want.dts, want.pts, want.data.len()),
            "sample {index}"
        );
        let data = src
            .span(sample.offset, u64::from(sample.size))
            .expect("the sample bytes");
        assert_eq!(data, want.data, "sample {index} bytes");
    }
}

// ------------------------------------------------------------------ //
// The writer.
// ------------------------------------------------------------------ //

#[test]
fn the_writer_agrees_with_ffmpeg_sample_by_sample() {
    let reference = Reference::read();
    let mut muxer = Muxer::video(
        Video {
            kind: *b"avc1",
            width: reference.width,
            height: reference.height,
            config: reference.avcc.clone(),
        },
        1,
        reference.timescale as i32,
    )
    .expect("a muxer from the reference's own sample entry");

    let init = muxer.init_segment();
    let track = Track::from_init(&init).expect("the init segment reads");
    assert_eq!(track.timescale, reference.timescale, "init timescale");
    assert_eq!(
        track.entry.config, reference.avcc,
        "the record crosses whole"
    );
    assert_eq!(
        track.entry.visual,
        Some(Visual {
            width: reference.width,
            height: reference.height
        })
    );
    assert!(
        init.windows(4).any(|four| four == b"trex"),
        "an init segment declares its fragment defaults"
    );

    // The reference's own samples, in decode order, fed back in. The
    // bytes are stored framing and cross unchanged: this crate does no
    // reframing, because it knows nothing about NAL units.
    let mut ours: Vec<Fragment> = Vec::new();
    for sample in &reference.samples {
        ours.extend(
            muxer
                .push(Packet {
                    pts: sample.pts,
                    dts: Some(sample.dts),
                    // The reordering case: no duration on the wire, so
                    // the writer's lookahead is the path under test.
                    duration: None,
                    keyframe: sample.keyframe,
                    data: &sample.data,
                })
                .expect("push"),
        );
    }
    ours.extend(muxer.finish().expect("finish"));

    assert_eq!(
        ours.len(),
        reference.samples.len(),
        "one fragment per reference sample"
    );
    let read = Track::from_init(&init).expect("the init segment reads");
    for (index, (mine, theirs)) in ours.iter().zip(&reference.samples).enumerate() {
        assert_eq!(mine.sequence, index as u32 + 1, "mfhd sequence");
        assert_eq!(mine.base_decode_time, theirs.dts as u64, "tfdt of {index}");
        assert_eq!(mine.keyframe, theirs.keyframe, "sync flag of {index}");
        // Read back with this crate's own reader rather than trusted.
        let samples = read.fragment_samples(&mine.bytes).expect("a fragment");
        assert_eq!(samples.len(), 1, "sample count of fragment {index}");
        let one = samples[0];
        assert_eq!(one.size as usize, theirs.data.len(), "size of {index}");
        assert_eq!(
            one.duration,
            i64::from(theirs.duration),
            "duration of {index}"
        );
        assert_eq!(one.pts - one.dts, theirs.pts - theirs.dts, "cts of {index}");
        assert_eq!(one.keyframe, theirs.keyframe, "sync flag of {index}");
        assert_eq!(
            sample_bytes(&mine.bytes, 0, &one).expect("the bytes"),
            &theirs.data[..],
            "mdat payload of sample {index} is not byte identical"
        );
    }

    // And the whole stream decodes, when ffprobe is here to say so.
    let mut stream = init;
    for fragment in &ours {
        stream.extend_from_slice(&fragment.bytes);
    }
    if let Some(frames) = ffprobe_frames(&stream) {
        assert_eq!(
            frames, 60,
            "the reference is two seconds at thirty a second"
        );
    }
}

#[test]
fn a_cut_mode_writer_makes_the_group_sized_fragments_ffmpeg_makes() {
    // The other mode: cut where the reference cut, so the file this
    // crate writes has the same shape as the one ffmpeg wrote, a whole
    // group of pictures to a moof with a many-sample trun.
    let reference = Reference::read();
    let mut muxer = Muxer::video(
        Video {
            kind: *b"avc1",
            width: reference.width,
            height: reference.height,
            config: reference.avcc.clone(),
        },
        1,
        reference.timescale as i32,
    )
    .expect("a muxer")
    .with_mode(Mode::PerCut);

    let cuts: Vec<usize> = reference
        .fragments
        .iter()
        .map(|fragment| fragment.samples.len())
        .collect();
    let mut ours = Vec::new();
    let mut taken = 0usize;
    let mut cut = 0usize;
    for sample in &reference.samples {
        muxer
            .push(Packet {
                pts: sample.pts,
                dts: Some(sample.dts),
                duration: Some(i64::from(sample.duration)),
                keyframe: sample.keyframe,
                data: &sample.data,
            })
            .expect("push");
        taken += 1;
        if cut < cuts.len() && taken == cuts[cut] {
            ours.extend(muxer.cut().expect("a cut"));
            taken = 0;
            cut += 1;
        }
    }
    ours.extend(muxer.finish().expect("finish"));
    assert_eq!(
        ours.len(),
        reference.fragments.len(),
        "one fragment a group"
    );

    let init = muxer.init_segment();
    let track = Track::from_init(&init).expect("the init segment reads");
    let mut read = Vec::new();
    for fragment in &ours {
        for sample in track.fragment_samples(&fragment.bytes).expect("a fragment") {
            let data = sample_bytes(&fragment.bytes, 0, &sample).expect("the bytes");
            read.push((sample, data.to_vec()));
        }
    }
    assert_eq!(read.len(), reference.samples.len());
    for (index, ((sample, data), want)) in read.iter().zip(&reference.samples).enumerate() {
        assert_eq!(
            (sample.dts, sample.pts, sample.keyframe),
            (want.dts, want.pts, want.keyframe),
            "sample {index}"
        );
        assert_eq!(data, &want.data, "sample {index} bytes");
    }

    let mut stream = init;
    for fragment in &ours {
        stream.extend_from_slice(&fragment.bytes);
    }
    if let Some(frames) = ffprobe_frames(&stream) {
        assert_eq!(frames, 60, "ffmpeg decodes what this wrote");
    }
}

// ------------------------------------------------------------------ //
// Bad bytes, over a file with real structure in it.
// ------------------------------------------------------------------ //

mod common;
use common::every_parser;

#[test]
fn every_truncation_of_the_reference_is_refused_rather_than_survived() {
    let whole = bytes();
    for cut in 0..whole.len() {
        every_parser(&whole[..cut]);
    }
}

#[test]
fn random_damage_to_the_reference_is_refused_rather_than_survived() {
    let whole = bytes();
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        seed >> 11
    };
    for _ in 0..3000 {
        let mut damaged = whole.clone();
        // Flips land in the first few kilobytes as often as anywhere,
        // because that is where the structure is.
        for _ in 0..1 + next() % 8 {
            let at = if next() % 2 == 0 {
                (next() as usize) % damaged.len().min(4096)
            } else {
                (next() as usize) % damaged.len()
            };
            damaged[at] ^= 1 << (next() % 8);
        }
        every_parser(&damaged);
    }
}

#[test]
fn a_file_of_nothing_much_is_refused_rather_than_survived() {
    for data in [
        Vec::new(),
        vec![0u8; 8],
        vec![0xffu8; 64],
        b"ftyp".to_vec(),
        // A box claiming the whole of a 64-bit address space.
        [1u8, 0, 0, 0]
            .iter()
            .chain(b"moov")
            .chain(&[0xffu8; 8])
            .copied()
            .collect(),
        // A box of declared size zero at the top level, which runs to
        // the end of whatever it is in.
        [0u8, 0, 0, 0].iter().chain(b"mdat").copied().collect(),
    ] {
        every_parser(&data);
    }
}

// ------------------------------------------------------------------ //
// This file's own parser, which nothing under test is involved in.
// ------------------------------------------------------------------ //

struct Reference {
    timescale: u32,
    width: u32,
    height: u32,
    avcc: Vec<u8>,
    fragments: Vec<ParsedFragment>,
    /// Every sample of every fragment, in decode order.
    samples: Vec<RefSample>,
}

struct RefSample {
    data: Vec<u8>,
    dts: i64,
    pts: i64,
    duration: u32,
    keyframe: bool,
}

struct ParsedFragment {
    sequence: u32,
    base_decode_time: u64,
    samples: Vec<ParsedSample>,
    mdat: Vec<u8>,
}

#[derive(Clone, Copy)]
struct ParsedSample {
    duration: u32,
    size: u32,
    flags: u32,
    cts: i64,
}

impl Reference {
    fn read() -> Self {
        let whole = bytes();
        // Cut the file by hand: every top-level box, in order.
        let mut init = Vec::new();
        let mut pieces: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let mut pos = 0usize;
        let mut pending_moof: Option<Vec<u8>> = None;
        while pos + 8 <= whole.len() {
            let size = be32(&whole, pos) as usize;
            assert!(size >= 8 && pos + size <= whole.len(), "a box at {pos}");
            let kind = &whole[pos + 4..pos + 8];
            let body = whole[pos..pos + size].to_vec();
            match kind {
                b"moof" => pending_moof = Some(body),
                b"mdat" => {
                    let moof = pending_moof.take().expect("an mdat after a moof");
                    pieces.push((moof, body));
                }
                _ if pending_moof.is_none() && pieces.is_empty() => init.extend_from_slice(&body),
                _ => {}
            }
            pos += size;
        }
        let fragments: Vec<ParsedFragment> = pieces
            .iter()
            .map(|(moof, mdat)| parse_fragment(moof, mdat))
            .collect();

        let timescale = be32(
            find_box(&init, &[b"moov", b"trak", b"mdia", b"mdhd"]).expect("an mdhd"),
            12,
        );
        let stsd = find_box(
            &init,
            &[b"moov", b"trak", b"mdia", b"minf", b"stbl", b"stsd"],
        )
        .expect("an stsd");
        let avc1 = child_box(&stsd[8..], b"avc1").expect("an avc1 sample entry");
        let avcc = child_box(&avc1[78..], b"avcC")
            .expect("an avcC in the sample entry")
            .to_vec();
        let (width, height) = (be16(avc1, 24) as u32, be16(avc1, 26) as u32);

        // Decode order is fragment order; times chain across fragments.
        let mut samples = Vec::new();
        for fragment in &fragments {
            let mut dts = fragment.base_decode_time as i64;
            let mut offset = 0usize;
            for sample in &fragment.samples {
                let data = fragment.mdat[offset..offset + sample.size as usize].to_vec();
                offset += sample.size as usize;
                samples.push(RefSample {
                    data,
                    dts,
                    pts: dts + sample.cts,
                    duration: sample.duration,
                    keyframe: sample.flags & NON_SYNC == 0,
                });
                dts += sample.duration as i64;
            }
        }

        Self {
            timescale,
            width,
            height,
            avcc,
            fragments,
            samples,
        }
    }
}

/// One `moof` and its `mdat` taken apart, the sample table resolved by
/// hand: trun first-sample flags, per-sample fields, tfhd defaults.
fn parse_fragment(moof_box: &[u8], mdat_box: &[u8]) -> ParsedFragment {
    let moof = &moof_box[8..];
    let mdat = mdat_box[8..].to_vec();
    let sequence = be32(child_box(moof, b"mfhd").expect("an mfhd"), 4);
    let traf = child_box(moof, b"traf").expect("a traf");

    let tfhd = child_box(traf, b"tfhd").expect("a tfhd");
    let tfhd_flags = be32(tfhd, 0) & 0x00ff_ffff;
    let mut pos = 8usize; // version/flags + track_ID
    if tfhd_flags & 0x1 != 0 {
        pos += 8; // base_data_offset
    }
    if tfhd_flags & 0x2 != 0 {
        pos += 4; // sample_description_index
    }
    let default_duration = (tfhd_flags & 0x8 != 0).then(|| {
        let value = be32(tfhd, pos);
        pos += 4;
        value
    });
    let default_size = (tfhd_flags & 0x10 != 0).then(|| {
        let value = be32(tfhd, pos);
        pos += 4;
        value
    });
    let default_flags = (tfhd_flags & 0x20 != 0).then(|| be32(tfhd, pos));

    let tfdt = child_box(traf, b"tfdt").expect("a tfdt");
    let base_decode_time = match tfdt[0] {
        1 => u64::from_be_bytes(tfdt[4..12].try_into().expect("8 bytes")),
        _ => be32(tfdt, 4) as u64,
    };

    let trun = child_box(traf, b"trun").expect("a trun");
    let version = trun[0];
    let trun_flags = be32(trun, 0) & 0x00ff_ffff;
    let count = be32(trun, 4) as usize;
    let mut pos = 8usize;
    if trun_flags & 0x1 != 0 {
        pos += 4; // data_offset
    }
    let first_flags = (trun_flags & 0x4 != 0).then(|| {
        let value = be32(trun, pos);
        pos += 4;
        value
    });
    let mut samples = Vec::with_capacity(count);
    for index in 0..count {
        let duration = if trun_flags & 0x100 != 0 {
            let value = be32(trun, pos);
            pos += 4;
            value
        } else {
            default_duration.expect("a duration somewhere")
        };
        let size = if trun_flags & 0x200 != 0 {
            let value = be32(trun, pos);
            pos += 4;
            value
        } else {
            default_size.expect("a size somewhere")
        };
        let own_flags = if trun_flags & 0x400 != 0 {
            let value = be32(trun, pos);
            pos += 4;
            Some(value)
        } else {
            None
        };
        let flags = match (index, first_flags, own_flags) {
            (0, Some(first), _) => first,
            (_, _, Some(own)) => own,
            _ => default_flags.expect("sample flags somewhere"),
        };
        let cts = if trun_flags & 0x800 != 0 {
            let raw = be32(trun, pos);
            pos += 4;
            // Signed either version, which is the rule this crate
            // settled on; the reference is version 1 anyway.
            let _ = version;
            raw as i32 as i64
        } else {
            0
        };
        samples.push(ParsedSample {
            duration,
            size,
            flags,
            cts,
        });
    }

    ParsedFragment {
        sequence,
        base_decode_time,
        samples,
        mdat,
    }
}

/// The payload of the first `kind` child at each step of `path`.
fn find_box<'a>(mut data: &'a [u8], path: &[&[u8; 4]]) -> Option<&'a [u8]> {
    for kind in path {
        data = child_box(data, kind)?;
    }
    Some(data)
}

/// The payload of the first `kind` box among `data`'s top-level boxes.
fn child_box<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    let mut pos = 0usize;
    while pos + 8 <= data.len() {
        let size = be32(data, pos) as usize;
        if size < 8 || pos + size > data.len() {
            return None;
        }
        if &data[pos + 4..pos + 8] == kind {
            return Some(&data[pos + 8..pos + size]);
        }
        pos += size;
    }
    None
}

fn be32(data: &[u8], pos: usize) -> u32 {
    u32::from_be_bytes(data[pos..pos + 4].try_into().expect("4 bytes"))
}

fn be16(data: &[u8], pos: usize) -> u16 {
    u16::from_be_bytes(data[pos..pos + 2].try_into().expect("2 bytes"))
}

/// Frame count by ffprobe over bytes this crate wrote; `None` without
/// ffprobe on the PATH, where the byte comparisons above still hold.
fn ffprobe_frames(stream: &[u8]) -> Option<u64> {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let nonce = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("ffrwd-bmff-ref-{}-{nonce}", std::process::id()));
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("ours.mp4");
    std::fs::write(&path, stream).ok()?;
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-count_frames", "-select_streams", "v:0"])
        .args(["-show_entries", "stream=nb_read_frames"])
        .args(["-of", "default=nw=1:nk=1"])
        .arg(&path)
        .output()
        .ok()?;
    std::fs::remove_dir_all(&dir).ok();
    assert!(
        output.status.success(),
        "ffprobe rejected what this crate wrote: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

//! The crate against files real ffmpeg wrote.
//!
//! Nothing here is committed: every fixture is made by ffmpeg when the
//! tests run, once for the whole binary, and every test that needs one
//! skips itself with a message when ffmpeg is not on the PATH. What is
//! asserted about a file is asked of ffprobe rather than assumed, so a
//! change in what ffmpeg writes shows up as a disagreement rather than
//! as a test that still passes over a file nobody has.
//!
//! The fixtures, per codec, and what each is here to prove:
//!
//! | file | what it is for |
//! | --- | --- |
//! | `<c>.mp4` | the ordinary case: one edit list, `ctts`, sample tables |
//! | `<c>-fast.mp4` | `+faststart`: the `moov` in front of the `mdat` |
//! | `<c>-frag.mp4` | `frag_keyframe+empty_moov`: `moof`, `trun`, an explicit `base_data_offset` |
//! | `<c>-base.mp4` | the same plus `+default_base_moof`: §8.8.7's second case |
//! | `<c>-neg.mp4` | `+negative_cts_offsets`: composition offsets below zero |
//! | `<c>-cut.mp4` | `-ss 1 -c copy`: a file that begins mid-stream |
//! | `h264-raw.mp4` | a `media_time` that lands between two samples |
//! | `av.mp4`, `av-frag.mp4` | video and AAC together: two tracks, and two `traf` boxes to a `moof` |
//! | `big.mp4` | ten seconds of 640x360 with noise in it, so that "reading the front of each keyframe costs a fraction of the file" is a claim about real pictures |

use std::collections::BTreeMap;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use ffrwd_bmff::fragment::sample_bytes;
use ffrwd_bmff::mux::{Muxer, Packet, Video};
use ffrwd_bmff::patch::{self, Placed, Selector};
use ffrwd_bmff::scanner::{Scanner, Segment};
use ffrwd_bmff::source::Source;
use ffrwd_bmff::track::{self, Pick, Sample, Track};

// ------------------------------------------------------------------ //
// ffmpeg.
// ------------------------------------------------------------------ //

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg")
        .arg("-version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn try_ffmpeg(args: &[&str]) -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-nostdin", "-y", "-loglevel", "error"])
        .args(args)
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

fn ffmpeg(args: &[&str]) {
    assert!(try_ffmpeg(args), "ffmpeg {} failed", args.join(" "));
}

fn ffprobe(args: &[&str]) -> String {
    let output = Command::new("ffprobe")
        .args(["-hide_banner", "-v", "error"])
        .args(args)
        .output()
        .expect("ffprobe runs");
    assert!(
        output.status.success(),
        "ffprobe {} failed:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// Every packet of a stream, as the fields asked for, in decode order.
fn packets(path: &Path, stream: &str, entries: &str) -> Vec<Vec<String>> {
    ffprobe(&[
        "-select_streams",
        stream,
        "-show_entries",
        &format!("packet={entries}"),
        "-of",
        "csv=p=0",
        &text(path),
    ])
    .lines()
    .filter(|line| !line.trim().is_empty())
    .map(|line| {
        line.trim()
            .trim_end_matches(',')
            .split(',')
            .map(|field| field.to_string())
            .collect()
    })
    .collect()
}

/// Every packet's presentation time, in milliseconds, in decode order.
fn ffprobe_times(path: &Path) -> Vec<f64> {
    packets(path, "v:0", "pts_time")
        .iter()
        .filter_map(|row| row.first()?.parse::<f64>().ok())
        .map(|seconds| seconds * 1000.0)
        .collect()
}

fn text(path: &Path) -> String {
    path.to_str().expect("a path").to_string()
}

// ------------------------------------------------------------------ //
// The fixtures.
// ------------------------------------------------------------------ //

/// The codecs a fixture set is built for, and whether this ffmpeg could
/// build them.
struct Built {
    dir: PathBuf,
    codecs: Vec<&'static str>,
}

fn fixtures() -> Option<&'static Built> {
    static DIR: OnceLock<Option<Built>> = OnceLock::new();
    DIR.get_or_init(|| {
        if !have_ffmpeg() {
            return None;
        }
        let dir = std::env::temp_dir().join("ffrwd-bmff-fixtures");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let codecs = build(&dir);
        Some(Built { dir, codecs })
    })
    .as_ref()
}

fn at(name: &str) -> PathBuf {
    fixtures().expect("the fixtures").dir.join(name)
}

fn codecs() -> &'static [&'static str] {
    &fixtures().expect("the fixtures").codecs
}

/// One encode per codec, then the shapes of it, by remuxing with
/// `-c copy` so the pictures are the encoder's own bytes.
fn build(dir: &Path) -> Vec<&'static str> {
    let recipes: [(&'static str, &'static str, Vec<&'static str>); 3] = [
        (
            "h264",
            "testsrc2=size=320x180:rate=30",
            vec![
                "-c:v", "libx264", "-preset", "veryfast", "-g", "30", "-bf", "2",
            ],
        ),
        (
            "hevc",
            "testsrc2=size=320x180:rate=30",
            vec![
                "-c:v",
                "libx265",
                "-preset",
                "ultrafast",
                "-x265-params",
                "keyint=30:min-keyint=30:bframes=2:log-level=error",
            ],
        ),
        (
            "av1",
            "testsrc2=size=320x176:rate=30",
            vec![
                "-c:v",
                "libsvtav1",
                "-preset",
                "10",
                "-crf",
                "40",
                "-g",
                "30",
            ],
        ),
    ];

    let mut made = Vec::new();
    for (name, source, encoder) in recipes {
        let base = text(&dir.join(format!("{name}.mp4")));
        let mut args = vec!["-f", "lavfi", "-i", source, "-t", "2"];
        args.extend(encoder.iter().copied());
        args.extend(["-pix_fmt", "yuv420p"]);
        args.push(&base);
        if !try_ffmpeg(&args) {
            println!("skipping {name}: this ffmpeg will not encode it");
            continue;
        }
        for (suffix, flags) in [
            ("fast", vec!["-movflags", "+faststart"]),
            ("frag", vec!["-movflags", "frag_keyframe+empty_moov"]),
            (
                "base",
                vec!["-movflags", "frag_keyframe+empty_moov+default_base_moof"],
            ),
            ("neg", vec!["-movflags", "+negative_cts_offsets"]),
        ] {
            let out = text(&dir.join(format!("{name}-{suffix}.mp4")));
            let mut args = vec!["-i", &base, "-c", "copy"];
            args.extend(flags.iter().copied());
            args.push(&out);
            ffmpeg(&args);
        }
        let cut = text(&dir.join(format!("{name}-cut.mp4")));
        ffmpeg(&["-ss", "1", "-i", &base, "-c", "copy", &cut]);
        made.push(name);
    }

    if made.contains(&"h264") {
        let base = text(&dir.join("h264.mp4"));
        // A media time that lands between two samples: a raw stream has
        // no timestamps, so ffmpeg gives it a nominal rate the sample
        // durations themselves do not quite keep to.
        let raw = text(&dir.join("h264.h264"));
        ffmpeg(&[
            "-i",
            &base,
            "-c",
            "copy",
            "-bsf:v",
            "h264_mp4toannexb",
            "-f",
            "h264",
            &raw,
        ]);
        ffmpeg(&["-i", &raw, "-c", "copy", &text(&dir.join("h264-raw.mp4"))]);

        // Video and sound in one file: two tracks, and in the
        // fragmented shape two `traf` boxes to a `moof`.
        let av = text(&dir.join("av.mp4"));
        ffmpeg(&[
            "-i",
            &base,
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-t",
            "2",
            "-c:v",
            "copy",
            "-c:a",
            "aac",
            "-shortest",
            &av,
        ]);
        ffmpeg(&[
            "-i",
            &av,
            "-c",
            "copy",
            "-movflags",
            "frag_keyframe+empty_moov+default_base_moof",
            &text(&dir.join("av-frag.mp4")),
        ]);

        // Ten seconds of 640x360 with noise over it, so the pictures
        // are the size a real file's are.
        ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=640x360:rate=30",
            "-t",
            "10",
            "-vf",
            "noise=alls=40:allf=t+u",
            "-c:v",
            "libx264",
            "-preset",
            "veryfast",
            "-g",
            "30",
            "-bf",
            "2",
            "-pix_fmt",
            "yuv420p",
            &text(&dir.join("big.mp4")),
        ]);
    }
    made
}

/// Every fixture of every codec that was built, by name.
fn every_shape() -> Vec<String> {
    let mut out = Vec::new();
    for codec in codecs() {
        for suffix in ["", "-fast", "-frag", "-base", "-neg", "-cut"] {
            out.push(format!("{codec}{suffix}.mp4"));
        }
    }
    if codecs().contains(&"h264") {
        out.push("h264-raw.mp4".into());
        out.push("av.mp4".into());
        out.push("av-frag.mp4".into());
    }
    out
}

fn source(name: &str) -> Source<Cursor<Vec<u8>>> {
    let bytes = std::fs::read(at(name)).expect("a fixture");
    Source::new(Cursor::new(bytes)).expect("a source")
}

fn video_of(name: &str) -> Track {
    let mut src = source(name);
    track::read(&mut src, Pick::Video).expect("a video track")
}

macro_rules! skip_without_ffmpeg {
    () => {
        if fixtures().is_none() {
            println!("skipping: ffmpeg is not on the PATH");
            return;
        }
    };
}

// ------------------------------------------------------------------ //
// Timing.
// ------------------------------------------------------------------ //

#[test]
fn every_sample_is_shown_when_ffprobe_says_it_is() {
    skip_without_ffmpeg!();
    for name in every_shape() {
        let track = video_of(&name);
        let wanted = ffprobe_times(&at(&name));
        assert_eq!(
            track.samples.len(),
            wanted.len(),
            "{name}: a different number of samples than ffprobe found"
        );
        for (sample, want) in track.samples.iter().zip(&wanted) {
            let got = track.ms(sample.pts) as f64;
            assert!(
                (got - want).abs() <= 1.0,
                "{name}: sample {} is at {got} and ffprobe says {want}",
                sample.index
            );
        }
    }
}

#[test]
fn the_files_that_do_not_start_at_zero_say_so() {
    skip_without_ffmpeg!();
    // A fragmented file written with an empty moov has no edit list, so
    // its first picture is shown a reorder delay in, and ffprobe agrees.
    let frag = video_of("h264-frag.mp4");
    assert_eq!(frag.start_shift, 0, "it has no edit list to shift it");
    assert!(frag.ms(frag.samples[0].pts) > 0);

    // The same content with a moov has one, and starts at zero.
    let whole = video_of("h264.mp4");
    assert_eq!(whole.ms(whole.samples[0].pts), 0);
    assert!(whole.start_shift < 0, "the edit list moved it back");

    // A raw stream remuxed gets a media time between two samples, and
    // the samples in front of it keep their negative times rather than
    // disappearing.
    let raw = video_of("h264-raw.mp4");
    assert!(
        raw.ms(raw.samples[0].pts) < 0,
        "the first sample is before the file's own zero"
    );
    assert!(
        raw.samples.iter().any(|sample| raw.ms(sample.pts) == 0),
        "and one sample lands exactly on it"
    );
}

#[test]
fn a_negative_composition_offset_comes_back_negative() {
    skip_without_ffmpeg!();
    // `+negative_cts_offsets` is the case the two old copies of this
    // code disagreed about: one read a version 0 offset unsigned, which
    // turns a frame shown a moment early into a frame shown two billion
    // ticks late. Both boxes are read signed now, and ffprobe's own
    // times are the check.
    for codec in codecs() {
        let name = format!("{codec}-neg.mp4");
        let track = video_of(&name);
        let reordered = track.samples.iter().any(|sample| sample.pts < sample.dts);
        if !reordered {
            // An all-intra encode has nothing to reorder, so there is
            // nothing for the sign to matter to.
            continue;
        }
        let wanted = ffprobe_times(&at(&name));
        for (sample, want) in track.samples.iter().zip(&wanted) {
            assert!(
                (track.ms(sample.pts) as f64 - want).abs() <= 1.0,
                "{name}: sample {} is at {} and ffprobe says {want}",
                sample.index,
                track.ms(sample.pts)
            );
        }
        // And the offsets really are below zero, not merely small.
        let least = track
            .samples
            .iter()
            .map(|sample| sample.pts - sample.dts)
            .min()
            .expect("samples");
        assert!(least < 0, "{name}: no composition offset below zero");
    }
}

// ------------------------------------------------------------------ //
// The sample map.
// ------------------------------------------------------------------ //

#[test]
fn the_sample_entry_and_its_record_come_out_of_the_file() {
    skip_without_ffmpeg!();
    // ffmpeg writes `hev1` for some HEVC muxes and `hvc1` for others,
    // and both are the same record in a different box; either is right.
    let wanted: BTreeMap<&str, Vec<[u8; 4]>> = [
        ("h264", vec![*b"avc1", *b"avc3"]),
        ("hevc", vec![*b"hvc1", *b"hev1"]),
        ("av1", vec![*b"av01"]),
    ]
    .into_iter()
    .collect();
    for codec in codecs() {
        let kinds = &wanted[codec];
        for suffix in ["", "-frag", "-base"] {
            let name = format!("{codec}{suffix}.mp4");
            let track = video_of(&name);
            assert!(
                kinds.contains(&track.entry.kind),
                "{name}: the sample entry is {:?}",
                String::from_utf8_lossy(&track.entry.kind)
            );
            assert!(
                !track.entry.config.is_empty(),
                "{name}: no decoder configuration record"
            );
            // avcC and hvcC open with a configurationVersion of 1;
            // an av1C opens with a marker bit and a version of 1,
            // which is 0x81.
            let opener = if *codec == "av1" { 0x81 } else { 0x01 };
            assert_eq!(
                track.entry.config[0], opener,
                "{name}: the record does not open with its version"
            );
            let visual = track.entry.visual.expect("a visual entry");
            assert!(visual.width >= 160 && visual.height >= 90, "{name}");
        }
    }
}

#[test]
fn the_keyframes_are_the_ones_ffprobe_flags() {
    skip_without_ffmpeg!();
    for name in every_shape() {
        let wanted: Vec<bool> = packets(&at(&name), "v:0", "flags")
            .iter()
            .map(|row| row[0].starts_with('K'))
            .collect();
        let track = video_of(&name);
        let got: Vec<bool> = track.samples.iter().map(|sample| sample.keyframe).collect();
        assert_eq!(got, wanted, "{name}");
    }
}

#[test]
fn every_sample_is_where_ffprobe_says_its_bytes_are() {
    skip_without_ffmpeg!();
    // The direct check on §8.8.7: a fragmented file with two tracks has
    // two `traf` boxes to a `moof`, and getting the base wrong puts one
    // track's samples on top of the other's. ffprobe prints each
    // packet's byte position, so the answer is not this crate's own.
    for (name, stream, pick) in [
        ("av-frag.mp4", "v:0", Pick::Video),
        ("av-frag.mp4", "a:0", Pick::Audio),
        ("h264-frag.mp4", "v:0", Pick::Video),
        ("h264.mp4", "v:0", Pick::Video),
        ("h264-fast.mp4", "v:0", Pick::Video),
    ] {
        // One field a call: ffprobe prints the entries it was asked for
        // in its own order, not the order of the asking.
        let places = packets(&at(name), stream, "pos");
        let sizes = packets(&at(name), stream, "size");
        let rows: Vec<(String, String)> = places
            .iter()
            .zip(&sizes)
            .map(|(pos, size)| (pos[0].clone(), size[0].clone()))
            .collect();
        let mut src = source(name);
        let track = track::read(&mut src, pick).expect("a track");
        assert_eq!(track.samples.len(), rows.len(), "{name} {stream}");
        for (sample, row) in track.samples.iter().zip(&rows) {
            let Ok(pos) = row.0.parse::<u64>() else {
                continue; // ffprobe did not say, so there is nothing to check.
            };
            let size: u32 = row.1.parse().expect("a size");
            assert_eq!(
                (sample.offset, sample.size),
                (pos, size),
                "{name} {stream}: sample {}",
                sample.index
            );
        }
    }
}

#[test]
fn reading_the_front_of_each_keyframe_costs_a_fraction_of_the_file() {
    skip_without_ffmpeg!();
    if !codecs().contains(&"h264") {
        return;
    }
    // What the reader is for: a caller after what rides in front of a
    // picture reads a few hundred bytes of each sync sample and nothing
    // else. The tally is what makes that a measured claim rather than a
    // hope, and it is the access pattern the index package's own prefix
    // scan is built on.
    let mut src = source("big.mp4");
    let track = track::read(&mut src, Pick::Video).expect("a track");
    src.reset_tally();
    let keys: Vec<Sample> = track
        .samples
        .iter()
        .copied()
        .filter(|sample| sample.keyframe)
        .collect();
    assert!(!keys.is_empty(), "no sync samples");
    for sample in &keys {
        let want = 256.min(sample.size as usize);
        let front = src.read_at(sample.offset, want).expect("a prefix");
        assert_eq!(front.len(), want);
    }
    let tally = src.tally();
    assert!(
        tally.len > 2_000_000,
        "the fixture is too small to mean anything"
    );
    assert!(
        tally.share() < 1.0,
        "a keyframe prefix scan read {:.3}% of the file ({} of {} bytes)",
        tally.share(),
        tally.bytes_read,
        tally.len
    );

    // And reading every sample whole really is the expensive one, so
    // the ratio above is a saving rather than an accident.
    src.reset_tally();
    for sample in track.samples.iter().take(200) {
        let _ = src
            .span(sample.offset, u64::from(sample.size))
            .expect("bytes");
    }
    assert!(src.tally().bytes_read > tally.bytes_read * 10);
}

// ------------------------------------------------------------------ //
// The streaming scanner over what ffmpeg wrote.
// ------------------------------------------------------------------ //

#[test]
fn the_scanner_and_the_file_reader_agree_over_every_fragmented_shape() {
    skip_without_ffmpeg!();
    for codec in codecs() {
        for suffix in ["-frag", "-base"] {
            let name = format!("{codec}{suffix}.mp4");
            let bytes = std::fs::read(at(&name)).expect("a fixture");
            let mut scanner = Scanner::new();
            scanner.push(&bytes).expect("a push");
            scanner.finish().expect("a clean end");

            let mut track = None;
            let mut read: Vec<(i64, i64, Vec<u8>)> = Vec::new();
            while let Some(segment) = scanner.poll() {
                match segment {
                    Segment::Init(init) => {
                        track = Some(Track::from_init(&init.bytes).expect("an init segment"))
                    }
                    Segment::Fragment(frag) => {
                        let track = track.as_ref().expect("the init segment comes first");
                        // The fragment's own stream offset, so an
                        // explicit base_data_offset resolves.
                        for sample in track
                            .fragment_samples_at(&frag.bytes, frag.offset)
                            .expect("a fragment")
                        {
                            let data =
                                sample_bytes(&frag.bytes, frag.offset, &sample).expect("the bytes");
                            read.push((sample.dts, sample.pts, data.to_vec()));
                        }
                    }
                }
            }

            let mut src = source(&name);
            let whole = track::read(&mut src, Pick::Video).expect("a track");
            assert_eq!(read.len(), whole.samples.len(), "{name}");
            for (index, ((dts, pts, data), sample)) in read.iter().zip(&whole.samples).enumerate() {
                assert_eq!((*dts, *pts), (sample.dts, sample.pts), "{name} {index}");
                let want = src
                    .span(sample.offset, u64::from(sample.size))
                    .expect("bytes");
                assert_eq!(data, &want, "{name} {index} bytes");
            }
        }
    }
}

// ------------------------------------------------------------------ //
// Round trip: written here, read here, and decoded by ffmpeg.
// ------------------------------------------------------------------ //

#[test]
fn what_this_crate_writes_ffmpeg_reads_back_frame_for_frame() {
    skip_without_ffmpeg!();
    for codec in codecs() {
        let name = format!("{codec}.mp4");
        let mut src = source(&name);
        let read = track::read(&mut src, Pick::Video).expect("a track");
        let visual = read.entry.visual.expect("a visual entry");

        let mut muxer = Muxer::video(
            Video {
                kind: read.entry.kind,
                width: visual.width,
                height: visual.height,
                config: read.entry.config.clone(),
            },
            1,
            read.timescale as i32,
        )
        .expect("a muxer");

        // The file's own samples, in decode order, stored framing kept.
        // The edit list shift comes back off, because the writer writes
        // no edit list and a shifted decode time is not the one the
        // samples were coded at.
        let mut stream = muxer.init_segment();
        let mut written = 0usize;
        for sample in &read.samples {
            let data = src
                .span(sample.offset, u64::from(sample.size))
                .expect("the bytes");
            for fragment in muxer
                .push(Packet {
                    pts: sample.pts - read.start_shift,
                    dts: Some(sample.dts - read.start_shift),
                    duration: Some(sample.duration),
                    keyframe: sample.keyframe,
                    data: &data,
                })
                .expect("push")
            {
                stream.extend_from_slice(&fragment.bytes);
                written += fragment.samples;
            }
        }
        for fragment in muxer.finish().expect("finish") {
            stream.extend_from_slice(&fragment.bytes);
            written += fragment.samples;
        }
        assert_eq!(written, read.samples.len(), "{name}: every sample written");

        // This crate's own reader, over what it wrote.
        let mut ours = Source::new(Cursor::new(stream.clone())).expect("a source");
        let back = track::read(&mut ours, Pick::Video).expect("a track");
        assert_eq!(back.samples.len(), read.samples.len(), "{name}");
        assert_eq!(back.entry.config, read.entry.config, "{name}");
        assert_eq!(back.timescale, read.timescale, "{name}");
        for (index, (mine, theirs)) in back.samples.iter().zip(&read.samples).enumerate() {
            assert_eq!(
                mine.pts - mine.dts,
                theirs.pts - theirs.dts,
                "{name}: composition offset of {index}"
            );
            assert_eq!(
                mine.keyframe, theirs.keyframe,
                "{name}: sync flag of {index}"
            );
            assert_eq!(mine.size, theirs.size, "{name}: size of {index}");
        }

        // And ffprobe's, which is the one that is not ours.
        let out = at(&format!("{codec}-written.mp4"));
        std::fs::write(&out, &stream).expect("a file");
        let theirs = ffprobe_times(&at(&name));
        let mine = ffprobe_times(&out);
        assert_eq!(mine.len(), theirs.len(), "{name}: packet count");
        // The written file has no edit list, so its zero is the first
        // composition time rather than the edit's; the intervals are
        // what must agree.
        let shift = mine[0] - theirs[0];
        for (index, (got, want)) in mine.iter().zip(&theirs).enumerate() {
            assert!(
                (got - shift - want).abs() <= 1.0,
                "{name}: packet {index} is at {got} and the source's is at {want}"
            );
        }
        let frames = ffprobe(&[
            "-v",
            "error",
            "-count_frames",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=nb_read_frames",
            "-of",
            "default=nw=1:nk=1",
            &text(&out),
        ]);
        assert_eq!(
            frames.trim().parse::<usize>().expect("a frame count"),
            read.samples.len(),
            "{name}: ffmpeg decoded a different number of frames than were written"
        );
    }
}

// ------------------------------------------------------------------ //
// Patching a file in place.
// ------------------------------------------------------------------ //

const UUID: [u8; 16] = [
    0x1e, 0x7a, 0x44, 0x9c, 0x63, 0x0f, 0x4b, 0x21, 0x9c, 0x5a, 0x11, 0x08, 0x77, 0x3e, 0x24, 0x66,
];

#[test]
fn a_box_goes_into_every_shape_of_file_and_comes_back_out() {
    skip_without_ffmpeg!();
    let selector = Selector::Uuid(UUID);
    let payload = b"anything at all".to_vec();
    for name in every_shape() {
        let before = std::fs::read(at(&name)).expect("a fixture");
        let mut file = Cursor::new(before.clone());
        let placed = patch::install(&mut file, selector, &payload).expect("a write");
        let after = file.into_inner();
        assert!(after.len() > before.len(), "{name}");

        let mut src = Source::new(Cursor::new(after.clone())).expect("a source");
        assert_eq!(
            patch::read(&mut src, selector)
                .expect("a read")
                .expect("a box"),
            payload,
            "{name}"
        );
        // One read near the end of the file is what finding it cost.
        assert!(
            src.tally().bytes_read < 8192,
            "{name}: finding the box read {} bytes",
            src.tally().bytes_read
        );

        // Everything the file already had is still where it was, apart
        // from a fragmented file's mfra, which moves by the box's own
        // length so that it stays last.
        match placed {
            Placed::Appended { at } => {
                assert_eq!(&after[..at as usize], &before[..], "{name}")
            }
            Placed::BeforeMfra { at, moved } => {
                assert_eq!(&after[..at as usize], &before[..at as usize], "{name}");
                assert_eq!(
                    &after[after.len() - moved as usize..],
                    &before[before.len() - moved as usize..],
                    "{name}: the mfra did not end up last"
                );
            }
            other => panic!("{name}: {other:?}"),
        }

        // And the samples still read exactly as they did.
        let mut before_src = Source::new(Cursor::new(before)).expect("a source");
        let mut after_src = Source::new(Cursor::new(after.clone())).expect("a source");
        assert_eq!(
            track::read(&mut before_src, Pick::Video)
                .expect("a track")
                .samples,
            track::read(&mut after_src, Pick::Video)
                .expect("a track")
                .samples,
            "{name}"
        );

        // And ffmpeg still plays it, with nothing new to say about it.
        let out = at(&format!("patched-{name}"));
        std::fs::write(&out, &after).expect("a file");
        assert_eq!(
            ffprobe_times(&out).len(),
            ffprobe_times(&at(&name)).len(),
            "{name}: the patched file lost packets"
        );
    }
}

// ------------------------------------------------------------------ //
// Bad bytes, over files with real structure in them.
// ------------------------------------------------------------------ //

mod common;
use common::every_parser;

#[test]
fn every_truncation_of_a_real_file_is_refused_rather_than_survived() {
    skip_without_ffmpeg!();
    for name in every_shape() {
        let bytes = std::fs::read(at(&name)).expect("a fixture");
        // Every truncation, thinned out past the first kilobytes: the
        // interesting cuts are inside the headers and the tables, and a
        // cut in the middle of a picture is the same cut a thousand
        // times over. Every byte of a small file is covered in
        // `reference.rs`.
        let cuts: Vec<usize> = (0..bytes.len().min(2048))
            .chain((2048..bytes.len()).step_by(97))
            .chain(bytes.len().saturating_sub(4096)..bytes.len())
            .collect();
        for cut in cuts {
            every_parser(&bytes[..cut]);
        }
    }
}

#[test]
fn random_damage_to_a_real_file_is_refused_rather_than_survived() {
    skip_without_ffmpeg!();
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        seed >> 11
    };
    for name in every_shape() {
        let bytes = std::fs::read(at(&name)).expect("a fixture");
        for _ in 0..200 {
            let mut damaged = bytes.clone();
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
}

# ffrwd-bmff

ISO base media files, read and written: MP4, MOV, 3GP and the
fragmented shape of the same boxes. Where every sample of a track is
and when it is shown, a byte stream cut into an init segment and
fragments, those fragments built from coded packets, and a top-level
box found, appended or replaced in a file in place. No dependencies,
no unsafe code, and no codec parsing, so a wasm module compiles it in
and its tests run on the host.

The code was consolidated from ffrwd's index and moq packages, which
had two copies of the box layer between them, drifted apart. Where the
copies disagreed, the decision and the reason are written down under
[the rules not to simplify](#the-rules-not-to-simplify), and
[MIGRATION.md](MIGRATION.md) says where each old function went and what
a caller will see change.

```rust
use ffrwd_bmff::source::Source;
use ffrwd_bmff::track::{self, Pick};

let mut src = Source::new(std::fs::File::open("clip.mp4")?)?;
let track = track::read(&mut src, Pick::Video)?;
for sample in &track.samples {
    println!("{} ms, {} bytes, key {}",
        track.ms(sample.pts), sample.size, sample.keyframe);
}
```

## Using it

```toml
[dependencies]
ffrwd-bmff = { git = "https://github.com/imbcmdth/ffrwd-bmff", tag = "v0.1.0" }
```

## What it does

Reading a file. `track::read` takes anything that implements `Read`
and `Seek` and returns one track: its timescale, its sample entry and
the decoder configuration record inside it, and every sample in decode
order with an offset, a size, a decode time, a presentation time, a
duration and a sync flag. A fragmented file and a plain one are the
same call, because the `moov`'s sample tables and each `moof`'s
`trun` entries describe the same thing. No sample's bytes are read;
what comes back is where they are, which a caller fetches through
`Source::span` or `Source::read_at` when and if it wants them.

Reading a stream. `scanner::Scanner` takes bytes a chunk at a time and
cuts them at top-level box boundaries into one init segment,
everything before the first `moof`, and then one fragment per `moof`
and the boxes through its `mdat`. Each `moof` is read far enough to
report its `mfhd` sequence number and whether its first sample is a
sync sample. `Track::from_init` reads the init segment and
`Track::fragment_samples_at` reads each fragment into the same
`Sample` type the file reader produces.

Writing. `mux::Muxer` builds the `ftyp`+`moov` a decoder reads before
any sample and then turns pushed packets into `moof`+`mdat` fragments.
It handles decode times that arrive late or negative: a stream that
reorders frames may open with no decode time at all, and the writer
holds those packets until one settles, synthesizes the prefix
backwards at the settled step, and shifts the whole timeline so the
first fragment starts at zero.

Patching. `patch::install` puts a top-level box into a file without
moving anything else, `patch::find` and `patch::read` get it back, and
`patch::rewrite` copies a file without one. The box is named by a
four-character type or a 16-byte `uuid`, and its payload is the
caller's.

## What it leaves to the caller

Codecs. A decoder configuration record, the `avcC`, `hvcC`, `av1C` or
`esds` payload a sample entry carries, is a byte slice this crate
hands out of a file and puts into a file it writes, and never reads.
Sample bytes are the same in both directions, in whatever framing that
record declares. Parsing the record, and the NAL units or OBUs of a
sample, is [ffrwd-nal](https://github.com/imbcmdth/ffrwd-nal)'s job,
and neither crate depends on the other. That split is also what lets
one muxer write `avc1`, `hvc1` and `av01`: with the record opaque, the
only difference between them is which four characters go on the box
around it.

Transports. The writer's default mode is one fragment per sample,
which is what a publisher that wants each frame to leave as it is
encoded asks for. That is a `Mode`, not a shape any type assumes:
`Mode::PerCut` holds settled samples until `Muxer::cut` closes one
fragment over them, which is how ffmpeg writes a whole group of
pictures to a `moof`. What a group is, when to rotate one and what a
catalog says about it are not here.

Container choice. There is no sniffer that decides between MP4 and
something else. A caller that reads more than one container knows
which one it has.

## The rules not to simplify

Every place the two source implementations disagreed was settled once,
and each of these has a test that fails if it is undone.

**All three box sizes, and no cap.** ISO/IEC 14496-12 §4.2 gives a box
a 32-bit `size`, a `size` of 1 with a 64-bit `largesize`, or a `size`
of 0 meaning the box runs to the end of its parent. All three are
read, at the top level and inside a body. One source copy refused a
`size` of 0 as unsupported and refused any box past `1 << 28` as
malformed. Both refusals are wrong about real files: ffmpeg writes a
size-0 `mdat` when it is muxing into a pipe and does not know the
length yet, and an `mdat` of a long film is larger than a quarter of a
gigabyte and is not damaged. The cap is gone. What replaces it is the
rule the other copy already had: a box is never read into memory
because of the size it declares. The file reader turns a size into a
span and reads the span through `Source`, which clamps it against the
file's real length and against `Limits::max_read`. The scanner, which
has no file to seek in and must hold what it emits, keeps a bound of
its own, `Limits::max_buffered_box`, and names it when a box exceeds
it. `mdat` never needs buffering in the reader, which is the point of
keeping the two bounds apart.

**`base_data_offset`, all three cases.** §8.8.7 says: an explicit
`base_data_offset` when the flag is set; otherwise the enclosing
`moof`'s first byte when `default-base-is-moof` is set; otherwise the
`moof`'s first byte for the movie fragment's first track fragment, and
the end of the preceding track fragment's data for every one after it.
Neither source copy had the third case. One took the `moof` start for
every `traf`, which lands the second track's samples on top of the
first's in a file written without either flag; the other refused a
`moof` with more than one `traf` at all. §8.8.8's companion rule is
implemented too: a `trun` without `data-offset-present` starts where
the previous run's data ended, not back at the base. The tests check
each sample's byte position against ffprobe's, on a file with a video
track and an audio track in the same fragments.

**A version 0 composition offset is signed.** §8.6.1.3's `ctts` and
§8.8.8's `trun` both have a field the standard calls unsigned in
version 0 and signed in version 1, and ffmpeg writes negative values
into version 0 boxes. One source copy read `ctts` signed and `trun`
unsigned, which is the worst of both. Both are signed here, in both
boxes, which is what ffmpeg's own reader does. A file that genuinely
meant an offset above 2^31 ticks would be misread, and no muxer writes
one: at 90 kHz that is thirteen hours of reordering.

**One conversion helper, rounding half away from zero.** Every
timestamp crosses through `time::rescale`, in `i128` so nothing wraps,
rounded to nearest with halves away from zero so a negative time is the
mirror image of a positive one, clamped rather than wrapped at the
ends. `time::ticks_to_micros` is that helper with the time base's
numerator folded into the target; it used to truncate, and truncating
differs by up to one microsecond.

**The edit list snaps.** ffprobe prints a packet's time after the
`elst` (§8.6.6) has moved it, so this does too, by the rule written out
at the top of `src/track.rs`: the file's zero is the composition time
of the first sample at or after the first non-empty edit's
`media_time`, not `media_time` itself. For an ordinary encode they are
the same number. They part company for raw H.264 remuxed into MP4,
where `media_time` lands between two samples and the difference is most
of a frame. A sample before the zero keeps its negative time rather
than being dropped, which is what ffprobe prints for it.

**The writer writes no `elst`.** It writes no `mfra` and no `sidx`
either. A file this crate wrote, read back by this crate, therefore has
a `start_shift` of zero and its composition times exactly as stored:
for a reordering stream the first sample is presented at its
composition offset rather than at zero. ffprobe says the same about
ffmpeg's own `frag_keyframe+empty_moov` output. The round-trip test
asserts the intervals between packet times rather than the absolute
ones for this reason.

**A track timescale of zero is refused; a movie timescale of zero is
not.** The media timescale (§8.4.2) divides every time the track has,
and a reader that quietly substitutes one has timestamps that mean
nothing. The movie timescale (§8.2.2) only ever rescales an edit
list's durations, so a zero there falls back to 1000 and the file stays
readable.

**Every bound is a default.** `Limits` holds all of them, `Source` and
`Scanner` each carry one, and the constants behind them are public. No
length out of a file becomes a `Vec` without being checked against the
bytes that exist first: `Bytes::count` is the only place a table header
becomes a reservation, and it refuses a count the box could not hold
before the reservation is asked for.

**Errors do not allocate.** One enum, three fault kinds plus I/O, every
message a `&'static str`, carrying the byte offset the trouble was
found at and the four characters of the box it was inside, filled in by
the innermost parser that knew them. This matters because the
truncation test runs every parser once for every byte of a file.

**`mp4-atom` is not a dependency.** Another muxer in this family uses
it, and for a crate that owns a whole media pipeline that is a
reasonable trade. This one is different in three ways the dependency
cannot be bent to. It builds for `wasm32-wasip2` inside a module where
every byte of dependency is carried; it reads files that are damaged,
truncated or hostile without allocating from a length the file chose,
which means the bound has to be a property of the reader rather than of
a decoded type; and it reads a multi-gigabyte `mdat` without the `mdat`
ever being a value. A library built around decoding an atom into a
struct answers a different question.

## Not here yet

An edit list of several non-empty entries, which ffmpeg implements by
cutting and repeating samples. The first non-empty entry decides and
the rest are ignored, which is said rather than guessed at because
nothing a muxer writes has one.

Writing `elst`, `mfra` or `sidx`, and writing more than one track to a
file. Reading handles multi-track files, including their fragments.

Audio other than AAC on the write side: `mp4a` with an `esds` is what
is implemented. Opus (`dOps`), VP9 (`vpcC`) and anything else have no
sample entry builder. Reading names an entry it does not know rather
than guessing, and hands back its four characters with an empty
configuration record.

`Mode::PerCut` goes a little further than either source implementation
needed, and it stays. The brief was that one fragment per sample must
be a documented mode rather than an assumption, which means the `trun`
writer has to take any number of entries; once it does, the mode that
uses that is a handful of lines, and it is what lets the round-trip
test read a many-sample `trun` written by this crate.

## Building and testing

```
cargo test
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo build --target wasm32-wasip2
```

`tests/data/ref-frag.mp4` is the only committed fixture, forty
kilobytes of real ffmpeg output, and the comment at the top of
`tests/reference.rs` holds the command that regenerates it byte for
byte. Everything `tests/ffmpeg.rs` needs is built by ffmpeg when the
tests run: one encode per codec that the installed ffmpeg can make,
then `+faststart`, `frag_keyframe+empty_moov`, the same plus
`+default_base_moof`, `+negative_cts_offsets`, a `-ss 1 -c copy` cut, a
raw Annex-B remux, and a file with video and AAC in it. Every test that
needs them skips itself with a message when ffmpeg is not on the PATH,
and a codec the installed ffmpeg will not encode is skipped by name
rather than failing.

What is asserted is asked of ffprobe rather than assumed: every
packet's presentation time to within a millisecond, every packet's sync
flag, every packet's byte position and size. What this crate writes is
read back by this crate and handed to ffprobe to decode and count.
Truncation at every byte of the committed fixture, thinned over the
generated ones, and random bit flips over all of them, go through every
parser entry point.

## License

Apache-2.0.

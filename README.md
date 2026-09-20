# ffrwd-bmff

ISO base media files, read and written: MP4, MOV, 3GP and the
fragmented shape of the same boxes. One box-walking layer under four
faces of the format, no dependencies, and no idea what a codec is.

```rust
use ffrwd_bmff::{source::Source, track::{self, Pick}};

let mut src = Source::new(std::fs::File::open("clip.mp4")?)?;
let track = track::read(&mut src, Pick::Video)?;
for sample in &track.samples {
    println!("{} ms, {} bytes, key {}",
        track.ms(sample.pts), sample.size, sample.keyframe);
}
```

The four faces. **[`track::read`]** walks a file over `Read + Seek` and
says where every sample is and when it is shown, without reading a
picture. **[`scanner::Scanner`]** takes a byte stream a chunk at a time
and cuts it into an init segment and one fragment per `moof`.
**[`mux::Muxer`]** builds the `ftyp`+`moov` a decoder needs and the
`moof`+`mdat` fragments after it. **[`patch`]** finds, appends or
replaces a top-level box in a file in place, which is how a file
carries something that is not media.

[`track::read`]: src/track.rs
[`scanner::Scanner`]: src/scanner.rs
[`mux::Muxer`]: src/mux.rs
[`patch`]: src/patch.rs

## What it does, and what it leaves to the caller

It does not know what a codec is. A decoder configuration record, the
`avcC`, `hvcC`, `av1C` or `esds` payload a sample entry carries, is an
opaque byte slice: this crate hands one out of a file and puts one into
a file it writes, and never looks inside. Sample bytes are the same, in
and out, in whatever framing the record declares. Reading that record,
and the NAL units or OBUs of a sample, is `ffrwd-nal`'s job, and the
two crates have no dependency either way. That split is not tidiness:
it is what lets one muxer write `avc1`, `hvc1` and `av01` in the same
three lines, because with the record opaque the only difference between
them is which four characters go on the box around it.

It does not know what a transport is either. The writer's default mode
is one fragment per sample, because that is what the publisher this
code came out of asks for, but that is a `Mode` and not a shape baked
into any type; `Mode::PerCut` writes a whole group of pictures to a
`moof`, the way ffmpeg does. What a group is, when to rotate one, what
a catalog says about it: none of that is here.

It does not know what is in the box `patch` writes. The caller supplies
a four-character type or a 16-byte `uuid` and a payload, and gets back
"it went on the end", "it went over the one that was there" or "it went
in front of the `mfra`".

**Why not `mp4-atom`.** The other muxer on this machine uses it, and
for a crate that owns a whole media pipeline that is a reasonable
trade. This one is different in three ways that the dependency cannot
be bent to. It has to build for `wasm32-wasip2` inside a module where
every byte of dependency is carried; it has to read files that are
damaged, truncated or hostile without allocating from a length the file
chose, which means the bound has to be a property of the reader rather
than of a decoded type; and it has to read a multi-gigabyte `mdat`
without the `mdat` ever being a value. A library built around "decode
this atom into a struct" answers a different question. A box header is
eight bytes of big-endian integer. There is nothing here worth a
dependency.

## The invariants not to simplify

This crate is a merge of two implementations that had drifted. Every
place they disagreed was settled once, on purpose, and each of these
has a test that fails if it is undone.

**All three box sizes, and no cap.** §4.2 gives a box a 32-bit `size`,
a `size` of 1 with a 64-bit `largesize`, or a `size` of 0 meaning "to
the end of the parent". All three are read, at the top level and inside
a body. The streaming copy used to refuse size 0 as unsupported and
refuse any box past `1 << 28` as malformed. Both are wrong about real
files: ffmpeg writes a size-0 `mdat` when it is muxing into a pipe and
does not know the length yet, and an `mdat` of a long film is larger
than a quarter of a gigabyte and is not damaged. The cap is gone. What
replaces it is the discipline that was already in the reader: a box is
never read into memory because of the size it declares. The file reader
turns a size into a span and reads the span through `Source`, which
clamps it against the file's real length and against
`Limits::max_read`. The scanner, which has no file to seek in and must
hold what it emits, keeps a bound of its own, `Limits::max_buffered_box`,
and says so by name when a box exceeds it. `mdat` never needs buffering
in the reader, and that is the point of the split.

**`base_data_offset`, all three cases.** §8.8.7 says: an explicit
`base_data_offset` when the flag is set; else the enclosing `moof`'s
first byte when `default-base-is-moof` is set; else the `moof`'s first
byte for the movie fragment's **first** track fragment and the end of
the preceding track fragment's data for every one after it. Neither old
copy had the third case: one took the `moof` start for every `traf`,
which lands the second track's samples on top of the first's, and the
other refused a `moof` with more than one `traf` at all. §8.8.8's
companion rule is implemented too: a `trun` without `data-offset-present`
starts where the previous run's data ended, not back at the base. The
ffmpeg tests check each sample's byte position against ffprobe's, on a
file with a video track and an audio track in the same fragments.

**A version 0 composition offset is signed.** §8.6.1.3's `ctts` and
§8.8.8's `trun` both have a field that the standard calls unsigned in
version 0 and signed in version 1, and ffmpeg writes negative values
into version 0 boxes. One old copy read `ctts` signed and `trun`
unsigned, which is the worst of both. Both are signed here, in both
boxes, which is what ffmpeg's own reader does. A file that genuinely
meant an offset above 2^31 ticks would be misread, and no muxer writes
one: at 90 kHz that is thirteen hours of reordering.

**One conversion helper, rounding half away from zero.** Every
timestamp crosses through `time::rescale`, in `i128` so nothing wraps,
rounded to nearest with halves away from zero so a negative time is the
mirror image of a positive one, clamped rather than wrapped at the
ends. `ticks_to_micros` is that helper with the time base's numerator
folded into the target; it used to truncate, and truncating differs by
up to one microsecond.

**The edit list snaps.** ffprobe prints a packet's time after the
`elst` has moved it, so this does too, by the rule written out at the
top of `src/track.rs`: the file's zero is the composition time of the
first sample at or after the first non-empty edit's `media_time`, not
`media_time` itself. For every ordinary encode they are the same
number. They part company for raw H.264 remuxed into MP4, where
`media_time` lands between two samples and the difference is most of a
frame. A sample before the zero keeps its negative time rather than
being dropped, which is also what ffprobe prints for it.

**The writer writes no `elst`.** It writes no `mfra` and no `sidx`
either. So a file this crate wrote, read back by this crate, has a
`start_shift` of zero and its composition times exactly as stored: for
a reordering stream the first sample is presented at its composition
offset rather than at zero. ffprobe says the same about ffmpeg's own
`frag_keyframe+empty_moov` output. It is the honest answer for a stream
that has no beginning to trim to, and the round-trip test asserts the
*intervals* between packet times, not the absolute ones, for exactly
this reason.

**A track timescale of zero is refused; a movie timescale of zero is
not.** The media timescale (§8.4.2) divides every time the track has,
and a reader that quietly substitutes one has timestamps that mean
nothing. The movie timescale (§8.2.2) only ever rescales an edit list's
durations, so a zero there falls back to 1000 and the file stays
readable.

**Every bound is a default.** `Limits` holds all of them, `Source` and
`Scanner` each carry one, and the constants behind them are public. No
length out of a file becomes a `Vec` without being checked against the
bytes that exist: `Bytes::count` is the only place a table header
becomes a reservation, and it refuses a count the box could not hold
before the reservation is asked for.

**Errors do not allocate.** One enum, three fault kinds plus I/O, every
message a `&'static str`, carrying the byte offset the trouble was
found at and the four characters of the box it was inside. The offset
is filled in by the innermost parser that knew it. This matters because
the truncation test runs every parser once for every byte of a file.

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
byte. Everything else `tests/ffmpeg.rs` needs is built by ffmpeg when
the tests run: one encode per codec that this ffmpeg can make, then
`+faststart`, `frag_keyframe+empty_moov`, the same plus
`+default_base_moof`, `+negative_cts_offsets`, a `-ss 1 -c copy` cut, a
raw Annex-B remux, and a file with video and AAC in it. Every test that
needs them skips itself with a message when ffmpeg is not on the PATH,
and a codec this ffmpeg will not encode is skipped by name rather than
failing.

What is asserted is asked of ffprobe rather than assumed: every
packet's presentation time to within a millisecond, every packet's sync
flag, every packet's byte position and size. What this crate writes is
read back by this crate *and* handed to ffprobe to decode and count.
Truncation at every byte of the committed fixture and thinned over the
generated ones, and random bit flips over all of them, go through every
parser entry point.

## License

Apache-2.0.

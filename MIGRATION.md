# Moving to ffrwd-bmff

Where each function in the ffrwd packages that this crate was
consolidated from now lives, and what changes when it moves. The
behaviour notes at the end of each section are the ones worth reading
before the move: they are cases where the same file now reads
differently, on purpose.

Everything that parses a decoder configuration record or the NAL units
and OBUs of a sample went to
[ffrwd-nal](https://github.com/imbcmdth/ffrwd-nal) instead, and the
tables below name the function there when that is where a caller
should now go.

## ffrwd-index

`ffrwd_index_container::mp4`, `::source` and `::write` become thin
wrappers or go away. The index format's own `UUID` and `FileIndex`
stay where they are: this crate takes a selector and a payload and
knows nothing about either. Matroska (`container::mkv`) is untouched,
and so is `container::scan`, whose prefix read stops at the first
coded slice and therefore needs NAL knowledge; it gets what it needs
from `Source::read_at`, `Sample::offset`, `Sample::size` and
`Track::ms`.

| was | is |
| --- | --- |
| `mp4::read(src)` | `track::read(&mut src, Pick::Video)` |
| `mp4::BoxHeader` | `boxes::BoxSpan` (`boxes::BoxHeader` is now the unresolved header) |
| `mp4::read_header`, `top_level` | `boxes::` the same |
| `mp4::children(body)`, `child`, `Child` | `boxes::children(body, at)`, `boxes::child`, `boxes::Child` |
| `mp4::MAX_SAMPLES`, `MAX_TOP_LEVEL`, `MAX_CHILDREN` | the same consts, and `Limits` to move them |
| `mp4::find_index_box(src)` | `patch::find(&mut src, Selector::Uuid(UUID))` |
| `mp4::read_index(src)` | `patch::read(&mut src, Selector::Uuid(UUID))` |
| `mp4::spot(src)` | `patch::spot(&mut src, Selector::Uuid(UUID))` |
| `mp4::IndexBox` | `patch::Found` |
| `mp4::Spot` | `patch::Spot` |
| `write::install(file, index)` | `patch::install(file, Selector::Uuid(UUID), index)` |
| `write::rewrite(src, out, index)` | `patch::rewrite(&mut src, &mut out, Selector::Uuid(UUID), index)` |
| `write::Placed`, `write::Truncate` | `patch::` the same |
| `ffrwd_index_core::index::mp4_uuid_box(bytes)` | `boxes::uuid_boxed(&UUID, bytes)`, or `Selector::Uuid(UUID).boxed(bytes)` |
| `source::Source`, `Tally`, `MAX_READ` | `source::` the same |
| `source::Bytes` | `boxes::Bytes`, and `Bytes::at(data, base)` to have errors carry a file offset |
| `VideoTrack` | `track::Track` |
| `VideoTrack::ms(t)` | `track.ms(t)`, or `time::ms(t, timescale)` |
| `VideoTrack.codec: TrackCodec` | `track.entry.kind: [u8; 4]` (`avc1`, `avc3`, `hvc1`, `hev1`, `av01`) |
| `VideoTrack.codec_private` | `track.entry.config` |
| `VideoTrack.length_size` | `ffrwd_nal::config::avcc_length_size(&track.entry.config)` or `::hvcc_length_size`, and `None` for AV1 |
| `VideoTrack.start_shift` | `track.start_shift` |
| `VideoTrack.samples`, `Sample` | `track.samples`, `track::Sample` (gains `dts` and `duration`) |
| `VideoTrack::scanned(scan)` | the caller's own filter on `sample.keyframe` |
| `Error::Format(&'static str)` | `Error::Format(Fault)` |
| `Error::Unsupported(String)` | `Error::Unsupported(Fault)`, no allocation |
| `Error::Io`, `Error::Codec` | `Error::Io`; there is no codec error, because there is no codec |
| `kind_of(src)` | gone; it chose between MP4 and Matroska, so it belongs with the crate that reads both |

### What an ffrwd-index caller will see change

- A `trun` version 0 `sample_composition_time_offset` above 2^31 now
  reads as a negative number rather than as a large positive one. This
  is the fix; `ctts` already read signed, and the two disagreeing was
  the bug.
- Rescaling an empty edit's duration between the movie and media
  timescales now rounds to nearest instead of truncating, so a result
  can differ by one media tick.
- A track whose `mdhd` timescale is zero is now refused instead of
  being read with the timescale clamped to 1. An `mvhd` timescale of
  zero still falls back to 1000.
- A `moof` with several `traf` boxes now places each track fragment's
  data by §8.8.7's three cases. A file written without
  `base-data-offset-present` and without `default-base-is-moof` used to
  give every track fragment the `moof` start; the second and later ones
  now carry on from where the previous one's data ended.
- A `trun` without `data-offset-present` now starts where the previous
  run's data ended rather than back at the track fragment's base.
- A box of declared size 0 is read as running to the end of its parent
  in every reader, not only at the top level.
- `Error` no longer carries a formatted `String`, so messages lose the
  interpolated codec four-character code and gain a machine-readable
  `Fault::at` and `Fault::kind`. `Display` still prints a sentence.
- `patch::install` refusing a box that is not last no longer mentions
  `--rewrite`: the crate does not know the tool's flags. The tool adds
  that sentence itself, and matches on `Error::Unsupported` with the
  offset the error carries.
- `Sample::dts` carries the same edit list shift `Sample::pts` does.

## ffrwd-moq

`moq_core::fmp4`, `::mux` and `::demux` become thin wrappers. The
group and catalog conventions stay in the package: they are what a
transport decides, not what a container says.

| was | is |
| --- | --- |
| `fmp4::Scanner`, `Segment`, `Init`, `Fragment` | `scanner::` the same, with `bytes: Vec<u8>` in place of `bytes::Bytes` |
| `fmp4::Scanner::push`, `poll`, `finish`, `consumed`, `trailer` | the same |
| `fmp4::moof_summary`, `MoofSummary` | `scanner::` the same |
| `fmp4::peek_box(buf, offset)` | `boxes::parse_header(data, at)` |
| `fmp4::child_boxes(data, offset)` | `boxes::children(data, at)` |
| `fmp4::box_payload(data, offset)` | `boxes::parse_header` plus the caller's own slice, or `boxes::find_in` |
| `fmp4::MAX_BOX` | `Limits::max_buffered_box` |
| `fmp4::Error { offset, what: String }` | `Error` with `Fault { what: &'static str, at, kind }` |
| `demux::Track::read(init)` | `track::Track::from_init(init)` |
| `demux::Track::samples(fragment)` | `track.fragment_samples(fragment)`, or `fragment_samples_at(fragment, offset)` when the fragment's stream position matters |
| `demux::Sample { dts, pts, duration, keyframe, data }` | `track::Sample { offset, size, dts, pts, duration, keyframe, .. }`, with the bytes from `fragment::sample_bytes(fragment, at, &sample)` |
| `demux::Track::length_size()` | `ffrwd_nal::config::avcc_length_size(&track.entry.config)` |
| `demux::Media::Video { width, height, avcc }` | `track.entry.kind`, `track.entry.visual`, `track.entry.config` |
| `demux::Media::Audio { sample_rate, channels, asc }` | `track.entry.audio` and `track.entry.config`, which is the `esds` descriptor chain |
| `demux::read_tfhd`, `read_tfdt`, `read_trun` | `fragment::` the same, public |
| `demux::trex_defaults` | `track.defaults: TrexDefaults` |
| `mux::Muxer::video(extradata, w, h, num, den)` | `ffrwd_nal::config::parse_parameter_sets(extradata)` then `ffrwd_nal::config::build_avcc(&sps, &pps)`, then `mux::Muxer::video(Video { kind: *b"avc1", width, height, config }, num, den)` |
| `mux::Muxer::audio(asc, rate, channels, num, den)` | `mux::Muxer::audio(Audio { sample_rate, channels, config: asc }, num, den)` |
| `mux::Muxer::avcc()`, `decoder_config()` | `muxer.config()` |
| `mux::Muxer::init_segment`, `push`, `finish` | the same |
| `mux::Packet { data: annexb }` | `ffrwd_nal::annexb::annexb_to_length_prefixed(bytes, length_size)` first; `Packet::data` is stored framing |
| `mux::Fragment.bytes`, `sequence`, `keyframe`, `pts` | the same, plus `base_decode_time` and `samples` |
| `mux::Fragment.starts_group` | gone; the package keeps its own `starts_a_group` and computes it from `fragment.keyframe` and `fragment.pts` |
| `mux::AUDIO_GROUP_SECONDS` | gone, same reason |
| `mux::ticks_to_micros` | `time::ticks_to_micros` |
| `avc::build_avcc`, `parse_parameter_sets` | `ffrwd_nal::config::` the same |
| `avc::annexb_to_avcc` | `ffrwd_nal::annexb::annexb_to_length_prefixed(bytes, length_size)` |
| `avc::avcc_to_annexb` | `ffrwd_nal::annexb::length_prefixed_to_annexb(sample, length_size)` |
| `avc::avcc_length_size` | `ffrwd_nal::config::avcc_length_size` |

### What an ffrwd-moq caller will see change

- The `1 << 28` per-box cap is gone. The bound is now
  `Limits::max_buffered_box`, which defaults to 64 MiB and is
  configurable through `Scanner::with_limits`, and exceeding it is an
  `Error::Unsupported` rather than a malformed-stream error.
- A box of declared size 0 is accepted. It runs to the end of the
  stream: the bytes accumulate and `Scanner::finish` closes the box and
  emits the segment that held it. It used to be refused as unsupported.
- A `moof` with several `traf` boxes is read instead of refused.
  `fragment_samples` returns the samples of the track it was called on
  and skips the rest.
- A fragment that carries nothing for the track returns an empty `Vec`
  instead of an error saying the fragment declares no samples.
- A fragment with no `tfdt` is timed from zero instead of being
  refused. A caller reading a stream of such fragments carries the
  clock itself.
- `fragment_samples` no longer copies sample bytes. It returns offsets
  and sizes, and `fragment::sample_bytes` borrows out of the fragment.
  A caller that wants an owned `Vec` calls `.to_vec()`.
- `ticks_to_micros` rounds to nearest instead of truncating, so a wire
  timestamp can differ by one microsecond.
- Errors carry a `&'static str` and a byte offset instead of a
  formatted `String`, so messages no longer interpolate the pts or the
  size that caused them.
- The `ftyp` compatible brands are now `iso5 isom <entry kind> mp41`.
  For `avc1` that is byte for byte what the package wrote before; for
  an HEVC or AV1 track it names that entry instead.
- The muxer refuses an empty packet and nothing else about its
  contents. It used to refuse a video packet with no Annex-B start
  code, which it can no longer see; `ffrwd_nal` refuses that when the
  caller reframes.

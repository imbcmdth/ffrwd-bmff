# Changelog

## 0.1.1

Added `Track::from_parts(handler, timescale, entry, samples)`, so a
track can be built without parsing ISO boxes. A reader of another
container, Matroska in the ffrwd index package, can now hand back the
same `Track` an MP4 read produces, and a routine that takes `&Track`
and reads samples out of a `Source` serves both containers instead of
needing a parallel view of its own. Its doc states the invariants the
caller owns: samples in decode order, offsets and sizes absolute in the
byte source they will be read from, and times in the track's own ticks.
The fields it does not take are left at the values that mean the file
said nothing, and they are public; `with_track_id` and
`with_start_shift` set the two a foreign reader usually has.

`Track::next_dts` is now a public method, derived from the last
sample's decode time plus its duration, where it was a private field.
Nothing about a parsed track changes: for a track read out of a `moov`
the derived number is what the `stts` deltas add up to.

`Track::fragment_samples` now documents what it does on a track built
from parts: it matches the `moof` against the track's `track_id` and
`defaults`, so a fragment naming another track comes back empty rather
than being refused.

No behaviour of the readers, the writer, the scanner or the patcher
changed.

## 0.1.0

First release. ISO base media files read and written: box walking,
sample tables, edit lists, movie fragments, a streaming segmenter, a
fragment writer and in-place patching of a top-level box. No
dependencies, no unsafe code, no codec parsing.

The code was consolidated from ffrwd's index and moq packages, which
had two copies of the box layer between them. Where the copies
disagreed, the reconciliation is in the README under "The rules not to
simplify", and MIGRATION.md says where each old function went and what
a caller will see change. The short version: all three box sizes
including a declared size of zero, no blanket cap on a box's length,
all three cases of `tfhd`'s base data offset, a version 0 composition
offset read as signed in both `ctts` and `trun`, one rational
conversion helper rounding half away from zero, and one error enum that
does not allocate.

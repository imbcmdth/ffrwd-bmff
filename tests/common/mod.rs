//! The one fuzz harness both test binaries use.
//!
//! A parser is only as good as the worst bytes anyone hands it, so
//! every entry point of the crate goes in one function and both the
//! committed fixture and the ffmpeg-built ones are pushed through it.

use ffrwd_bmff::source::Source;
use ffrwd_bmff::track::{self, Pick, Track};

/// Every parser entry point of the crate, over whatever bytes it is
/// given.
pub fn every_parser(data: &[u8]) {
    use ffrwd_bmff::boxes;
    use ffrwd_bmff::fragment;
    use ffrwd_bmff::patch::{self, Selector};
    use ffrwd_bmff::scanner;

    let _ = boxes::parse_header(data, 0);
    let _ = boxes::children(data, 0);
    let _ = boxes::find_in(data, b"moov", 0);
    let _ = scanner::moof_summary(data, 0);
    let _ = fragment::read_tfhd(data, 0);
    let _ = fragment::read_tfdt(data, 0);
    let _ = Track::all(data);
    let init = Track::from_init(data);

    let mut scanner = scanner::Scanner::new();
    let _ = scanner.push(data);
    while scanner.poll().is_some() {}
    let _ = scanner.finish();

    let mut src = Source::new(std::io::Cursor::new(data.to_vec())).expect("a source");
    let _ = boxes::top_level(&mut src);
    let _ = patch::find(&mut src, Selector::Kind(*b"moov"));
    let _ = patch::read(&mut src, Selector::Uuid([0x5a; 16]));
    let _ = patch::spot(&mut src, Selector::Uuid([0x5a; 16]));
    for pick in [Pick::Video, Pick::Audio, Pick::First] {
        if let Ok(found) = track::read(&mut src, pick) {
            for sample in found.samples.iter().take(64) {
                let _ = src.span(sample.offset, u64::from(sample.size));
            }
        }
    }
    if let Ok(track) = init {
        let _ = track.fragment_samples(data);
        let _ = track.fragment_samples_at(data, 12345);
    }
}

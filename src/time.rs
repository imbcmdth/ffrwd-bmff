//! One rational conversion, used by everything that has a clock.
//!
//! A file's times are counted in ticks of a timescale it declares
//! (§8.4.2 for a track, §8.2.2 for the movie), and every number a
//! caller wants out of them, be it a millisecond for a log line or a
//! microsecond for a wire field, is the same multiply and divide. Doing
//! it twice in two places is how two readers of the same file come to
//! disagree by a tick, so it is done here.
//!
//! **The arithmetic.** In `i128`, so a 64-bit tick count times a
//! timescale cannot wrap; rounded to nearest with halves going away
//! from zero, so a tick of 1/15360 lands where ffprobe's printed
//! decimal says it does and a negative time rounds the mirror image of
//! a positive one; clamped into `i64` at the ends rather than wrapping.
//!
//! Truncating instead, which is what the streaming side used to do on
//! the way to microseconds, differs by at most one unit of the output.
//! It differs, though, so this is the one that is right and the other
//! is gone.

/// `value` counted in `from` ticks a second, counted in `to` ticks a
/// second instead.
///
/// A `from` of zero is treated as one: a file that declares a timescale
/// of zero has said nothing, and refusing to divide is better than
/// dividing by it.
pub fn rescale(value: i64, from: u64, to: u64) -> i64 {
    let from = i128::from(from.max(1));
    let to = i128::from(to);
    let scaled = i128::from(value) * to;
    let rounded = if scaled >= 0 {
        (scaled + from / 2) / from
    } else {
        (scaled - from / 2) / from
    };
    rounded.clamp(i128::from(i64::MIN), i128::from(i64::MAX)) as i64
}

/// A tick count of a track's own timescale, as milliseconds: the number
/// ffprobe prints as `pts_time`, times a thousand.
pub fn ms(ticks: i64, timescale: u32) -> i64 {
    rescale(ticks, u64::from(timescale), 1000)
}

/// A tick count of a `num/den` time base, as the microseconds a wire
/// field counts, clamped at zero on the way into an unsigned one.
///
/// This is [`rescale`] with the numerator folded into the target: a
/// tick is `num/den` seconds, so it is `num * 1_000_000 / den`
/// microseconds.
pub fn ticks_to_micros(ticks: i64, time_base_num: i32, time_base_den: i32) -> u64 {
    if ticks <= 0 || time_base_num <= 0 || time_base_den <= 0 {
        return 0;
    }
    let to = u64::from(time_base_num.unsigned_abs()) * 1_000_000;
    rescale(ticks, u64::from(time_base_den.unsigned_abs()), to).max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tick_lands_where_ffprobe_prints_it() {
        // Two frames of a 15360 timescale: 0.066666..., which ffprobe
        // prints as 0.066667 and this rounds to 67 ms.
        assert_eq!(ms(1024, 15360), 67);
        assert_eq!(ms(0, 15360), 0);
        // And a negative time is the mirror image, not a floor.
        assert_eq!(ms(-1024, 15360), -67);
        assert_eq!(ms(-119999, 1_200_000), -100);
    }

    #[test]
    fn halves_go_away_from_zero() {
        assert_eq!(rescale(1, 2, 1), 1);
        assert_eq!(rescale(-1, 2, 1), -1);
        assert_eq!(rescale(3, 2, 1), 2);
        assert_eq!(rescale(-3, 2, 1), -2);
    }

    #[test]
    fn nothing_wraps_at_the_ends() {
        assert_eq!(rescale(i64::MAX, 1, 1_000_000), i64::MAX);
        assert_eq!(rescale(i64::MIN, 1, 1_000_000), i64::MIN);
        // A timescale of zero is a file saying nothing, not a divide by
        // zero.
        assert_eq!(rescale(5, 0, 1000), 5000);
    }

    #[test]
    fn ticks_scale_to_microseconds() {
        assert_eq!(ticks_to_micros(30, 1, 30), 1_000_000);
        assert_eq!(ticks_to_micros(2048, 1, 61440), 33_333);
        assert_eq!(ticks_to_micros(-5, 1, 30), 0);
        // A numerator that is not one multiplies on the way in.
        assert_eq!(ticks_to_micros(1, 1001, 30000), 33_367);
    }
}

//! Timestamp unwrapping: RTMP timestamps are 32-bit milliseconds and wrap after ~49.7 days.

/// Converts a sequence of wrapping 32-bit timestamps into monotonic-ish 64-bit ones.
///
/// Each timestamp is taken to be the nearer of the two ways from the one before:
/// forward (across a wrap) or back. A step back just after a wrap is then a step
/// back, not a jump of 2^32 ms ahead.
#[derive(Debug, Default, Clone)]
pub struct TsUnwrapper {
    last: Option<u32>,
    /// `last` unwrapped. Below zero after a step back from a start near zero.
    current: i64,
}

impl TsUnwrapper {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn unwrap(&mut self, ts: u32) -> u64 {
        self.current = match self.last {
            Some(last) => self.current + i64::from(ts.wrapping_sub(last) as i32),
            None => i64::from(ts),
        };
        self.last = Some(ts);
        self.current.max(0) as u64
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const WRAP: u64 = 1 << 32;

    #[test]
    fn wraps_forward_and_tolerates_small_jitter() {
        let mut u = TsUnwrapper::new();
        assert_eq!(u.unwrap(u32::MAX - 10), (u32::MAX - 10) as u64);
        assert_eq!(u.unwrap(u32::MAX - 20), (u32::MAX - 20) as u64);
        assert_eq!(u.unwrap(5), WRAP + 5);
    }

    #[test]
    fn a_step_back_across_the_wrap_is_a_step_back() {
        let mut u = TsUnwrapper::new();
        assert_eq!(u.unwrap(u32::MAX - 10), WRAP - 11);
        assert_eq!(u.unwrap(5), WRAP + 5);
        assert_eq!(u.unwrap(u32::MAX - 3), WRAP - 4);
        assert_eq!(u.unwrap(10), WRAP + 10);
        assert_eq!(u.unwrap(20), WRAP + 20);
    }

    #[test]
    fn large_steps_back_that_are_no_wrap() {
        let mut u = TsUnwrapper::new();
        assert_eq!(u.unwrap(3_000_000_000), 3_000_000_000);
        assert_eq!(u.unwrap(1_000_000_000), 1_000_000_000);
        // Back further than half the range: a wrap after all.
        let mut u = TsUnwrapper::new();
        assert_eq!(u.unwrap(4_000_000_000), 4_000_000_000);
        assert_eq!(u.unwrap(1_000_000_000), WRAP + 1_000_000_000);
    }

    #[test]
    fn a_step_back_from_the_start_stays_at_zero_and_recovers() {
        let mut u = TsUnwrapper::new();
        assert_eq!(u.unwrap(5), 5);
        assert_eq!(u.unwrap(u32::MAX - 3), 0);
        assert_eq!(u.unwrap(10), 10);
    }

    proptest! {
        /// Small steps either way, starting anywhere near the wrap, unwrap to the
        /// start plus the steps so far.
        #[test]
        fn small_steps_are_followed_across_the_wrap(
            start in (u32::MAX - 100_000)..=u32::MAX,
            steps in prop::collection::vec(-1000i64..=1000, 0..200),
        ) {
            let mut u = TsUnwrapper::new();
            let mut expected = i64::from(start);
            prop_assert_eq!(u.unwrap(start), start as u64);
            for step in steps {
                expected += step;
                let ts = expected.rem_euclid(1 << 32) as u32;
                prop_assert_eq!(u.unwrap(ts), expected as u64);
            }
        }
    }
}

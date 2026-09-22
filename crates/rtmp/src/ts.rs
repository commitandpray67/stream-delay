//! Timestamp unwrapping: RTMP timestamps are 32-bit milliseconds and wrap after ~49.7 days.

/// Converts a sequence of wrapping 32-bit timestamps into monotonic-ish 64-bit ones.
#[derive(Debug, Default, Clone)]
pub struct TsUnwrapper {
    last: Option<u32>,
    epoch: u64,
}

impl TsUnwrapper {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn unwrap(&mut self, ts: u32) -> u64 {
        if let Some(last) = self.last {
            // A large backwards jump means the counter wrapped.
            if ts < last && last - ts > u32::MAX / 2 {
                self.epoch += 1 << 32;
            }
        }
        self.last = Some(ts);
        self.epoch + ts as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wraps_forward_and_tolerates_small_jitter() {
        let mut u = TsUnwrapper::new();
        assert_eq!(u.unwrap(u32::MAX - 10), (u32::MAX - 10) as u64);
        assert_eq!(u.unwrap(u32::MAX - 20), (u32::MAX - 20) as u64);
        assert_eq!(u.unwrap(5), (1u64 << 32) + 5);
    }
}

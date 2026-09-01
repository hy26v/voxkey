// ABOUTME: Coalesces small realtime microphone callbacks into stable processing frames.
// ABOUTME: Bounds pending audio and preserves exact interleaved sample order across flushes.

use std::time::Duration;

pub struct PcmBatcher {
    channels: usize,
    target_samples: usize,
    pending: Vec<i16>,
}

impl PcmBatcher {
    pub fn new(sample_rate: u32, channels: u16, target_duration: Duration) -> Result<Self, String> {
        if sample_rate == 0 || channels == 0 {
            return Err("PCM batching requires a real sample rate and channel count".to_string());
        }
        let frames = u128::from(sample_rate)
            .saturating_mul(target_duration.as_micros())
            .div_ceil(1_000_000)
            .max(1);
        let target_samples = frames
            .saturating_mul(u128::from(channels))
            .min(usize::MAX as u128) as usize;
        Ok(Self {
            channels: usize::from(channels),
            target_samples,
            pending: Vec::with_capacity(target_samples),
        })
    }

    pub fn push(&mut self, samples: &[i16]) -> Result<Vec<Vec<i16>>, String> {
        if !samples.len().is_multiple_of(self.channels) {
            return Err(format!(
                "realtime audio has {} values, not a whole number of {}-channel frames",
                samples.len(),
                self.channels
            ));
        }
        let expected_batches = self
            .pending
            .len()
            .saturating_add(samples.len())
            .checked_div(self.target_samples)
            .unwrap_or(0);
        let mut batches = Vec::with_capacity(expected_batches);
        let mut remaining = samples;

        if !self.pending.is_empty() {
            let needed = self.target_samples - self.pending.len();
            let taken = needed.min(remaining.len());
            self.pending.extend_from_slice(&remaining[..taken]);
            remaining = &remaining[taken..];
            if self.pending.len() == self.target_samples {
                batches.push(std::mem::replace(
                    &mut self.pending,
                    Vec::with_capacity(self.target_samples),
                ));
            }
        }

        while remaining.len() >= self.target_samples {
            let (ready, rest) = remaining.split_at(self.target_samples);
            batches.push(ready.to_vec());
            remaining = rest;
        }
        self.pending.extend_from_slice(remaining);
        debug_assert!(self.pending.len() < self.target_samples);
        Ok(batches)
    }

    pub fn flush(&mut self) -> Option<Vec<i16>> {
        (!self.pending.is_empty())
            .then(|| std::mem::replace(&mut self.pending, Vec::with_capacity(self.target_samples)))
    }

    #[cfg(test)]
    fn pending_samples(&self) -> usize {
        self.pending.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hundreds_of_tiny_callbacks_become_stable_processing_batches() {
        let mut batcher = PcmBatcher::new(1_000, 1, Duration::from_millis(100)).unwrap();
        let mut batches = Vec::new();
        for sample in 0_i16..1_000 {
            batches.extend(batcher.push(&[sample]).unwrap());
        }

        assert_eq!(batches.len(), 10);
        assert!(batches.iter().all(|batch| batch.len() == 100));
        assert_eq!(batches.concat(), (0_i16..1_000).collect::<Vec<_>>());
        assert_eq!(batcher.pending_samples(), 0);
    }

    #[test]
    fn flush_preserves_the_final_partial_batch() {
        let mut batcher = PcmBatcher::new(16_000, 1, Duration::from_millis(100)).unwrap();
        assert!(batcher.push(&[1, 2, 3]).unwrap().is_empty());

        assert_eq!(batcher.flush(), Some(vec![1, 2, 3]));
        assert_eq!(batcher.flush(), None);
    }

    #[test]
    fn one_large_callback_is_split_without_reordering() {
        let mut batcher = PcmBatcher::new(1_000, 1, Duration::from_millis(100)).unwrap();
        let input = (0_i16..1_050).collect::<Vec<_>>();

        let batches = batcher.push(&input).unwrap();

        assert_eq!(batches.len(), 10);
        assert_eq!(batches.concat(), input[..1_000]);
        assert_eq!(batcher.flush().unwrap(), input[1_000..]);
    }

    #[test]
    fn interleaved_frames_are_never_split_between_batches() {
        let mut batcher = PcmBatcher::new(10, 2, Duration::from_millis(150)).unwrap();
        let batches = batcher.push(&[1, 2, 3, 4, 5, 6]).unwrap();

        assert_eq!(batches, [vec![1, 2, 3, 4]]);
        assert_eq!(batcher.flush(), Some(vec![5, 6]));
        assert!(batcher.push(&[1, 2, 3]).is_err());
    }
}

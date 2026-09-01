// ABOUTME: Adapts native microphone formats to Voxkey's configured PCM format.
// ABOUTME: Selects a supported device mode, converts samples, remixes channels, and resamples continuously.

use cpal::{
    FromSample, Sample, SampleFormat, SampleRate, SupportedStreamConfig, SupportedStreamConfigRange,
};

/// Choose the device mode that needs the least adaptation while still
/// preferring the configured channel count and sample rate.
pub fn select_input_config<I>(
    configs: I,
    target_rate: u32,
    target_channels: u16,
) -> Option<SupportedStreamConfig>
where
    I: IntoIterator<Item = SupportedStreamConfigRange>,
{
    configs
        .into_iter()
        .min_by_key(|range| config_score(range, target_rate, target_channels))
        .map(|range| {
            let rate = target_rate.clamp(range.min_sample_rate().0, range.max_sample_rate().0);
            range.with_sample_rate(SampleRate(rate))
        })
}

fn config_score(
    range: &SupportedStreamConfigRange,
    target_rate: u32,
    target_channels: u16,
) -> (u8, u8, u16, u32, u8) {
    let channels = range.channels();
    let selected_rate = target_rate.clamp(range.min_sample_rate().0, range.max_sample_rate().0);
    (
        u8::from(channels != target_channels),
        u8::from(selected_rate != target_rate),
        channels.abs_diff(target_channels),
        selected_rate.abs_diff(target_rate),
        sample_format_rank(range.sample_format()),
    )
}

fn sample_format_rank(format: SampleFormat) -> u8 {
    match format {
        SampleFormat::I16 => 0,
        SampleFormat::F32 => 1,
        SampleFormat::U16 => 2,
        SampleFormat::I32 => 3,
        SampleFormat::F64 => 4,
        SampleFormat::I24 => 5,
        SampleFormat::I8 | SampleFormat::U8 => 6,
        SampleFormat::I64 | SampleFormat::U32 | SampleFormat::U64 => 7,
        _ => 8,
    }
}

/// Stateful converter whose sample clock continues across CPAL callbacks.
/// Keeping one rational output position avoids callback-boundary gaps and
/// long-recording drift when the microphone runs at a different sample rate.
pub struct AudioAdapter {
    input_rate: u32,
    input_channels: usize,
    output_rate: u32,
    output_channels: usize,
    input_frame: u64,
    output_frame: u64,
    previous: Vec<f32>,
    current: Vec<f32>,
    initialized: bool,
}

impl AudioAdapter {
    pub fn new(
        input_rate: u32,
        input_channels: u16,
        output_rate: u32,
        output_channels: u16,
    ) -> Result<Self, String> {
        if input_rate == 0 || output_rate == 0 {
            return Err("audio sample rates must be greater than zero".to_string());
        }
        if input_channels == 0 || output_channels == 0 {
            return Err("audio channel counts must be greater than zero".to_string());
        }
        Ok(Self {
            input_rate,
            input_channels: usize::from(input_channels),
            output_rate,
            output_channels: usize::from(output_channels),
            input_frame: 0,
            output_frame: 0,
            previous: vec![0.0; usize::from(output_channels)],
            current: vec![0.0; usize::from(output_channels)],
            initialized: false,
        })
    }

    pub fn process_data(
        &mut self,
        data: &cpal::Data,
        format: SampleFormat,
    ) -> Result<Vec<i16>, String> {
        if format == SampleFormat::I16
            && self.input_rate == self.output_rate
            && self.input_channels == self.output_channels
        {
            let samples = data
                .as_slice::<i16>()
                .ok_or_else(|| "microphone returned data other than i16".to_string())?;
            if !samples.len().is_multiple_of(self.input_channels) {
                return Err("microphone returned an incomplete interleaved frame".to_string());
            }
            return Ok(samples.to_vec());
        }

        macro_rules! convert {
            ($sample:ty) => {
                self.process(
                    data.as_slice::<$sample>()
                        .ok_or_else(|| format!("microphone returned data other than {format}"))?,
                )
            };
        }

        match format {
            SampleFormat::I8 => convert!(i8),
            SampleFormat::I16 => convert!(i16),
            SampleFormat::I24 => convert!(cpal::I24),
            SampleFormat::I32 => convert!(i32),
            SampleFormat::I64 => convert!(i64),
            SampleFormat::U8 => convert!(u8),
            SampleFormat::U16 => convert!(u16),
            SampleFormat::U32 => convert!(u32),
            SampleFormat::U64 => convert!(u64),
            SampleFormat::F32 => convert!(f32),
            SampleFormat::F64 => convert!(f64),
            _ => Err(format!("unsupported microphone sample format {format}")),
        }
    }

    pub fn process<T>(&mut self, samples: &[T]) -> Result<Vec<i16>, String>
    where
        T: Copy,
        f32: FromSample<T>,
    {
        if !samples.len().is_multiple_of(self.input_channels) {
            return Err(format!(
                "microphone returned {} values, not a whole number of {}-channel frames",
                samples.len(),
                self.input_channels
            ));
        }

        let input_frames = samples.len() / self.input_channels;
        let estimated_frames = (input_frames as u128)
            .saturating_mul(u128::from(self.output_rate))
            .div_ceil(u128::from(self.input_rate))
            .saturating_add(2)
            .min(usize::MAX as u128) as usize;
        let mut output = Vec::with_capacity(estimated_frames.saturating_mul(self.output_channels));

        for frame in samples.chunks_exact(self.input_channels) {
            remix_frame(frame, &mut self.current);
            if !self.initialized {
                self.previous.copy_from_slice(&self.current);
                self.initialized = true;
                self.emit_current(&mut output);
                self.output_frame = 1;
                continue;
            }

            self.input_frame = self.input_frame.saturating_add(1);
            let interval_start = u128::from(self.input_frame.saturating_sub(1))
                .saturating_mul(u128::from(self.output_rate));
            let interval_end =
                u128::from(self.input_frame).saturating_mul(u128::from(self.output_rate));
            loop {
                let position =
                    u128::from(self.output_frame).saturating_mul(u128::from(self.input_rate));
                if position > interval_end {
                    break;
                }
                let fraction =
                    position.saturating_sub(interval_start) as f64 / f64::from(self.output_rate);
                for channel in 0..self.output_channels {
                    let previous = f64::from(self.previous[channel]);
                    let current = f64::from(self.current[channel]);
                    output.push(normalized_to_i16(
                        previous + (current - previous) * fraction,
                    ));
                }
                self.output_frame = self.output_frame.saturating_add(1);
            }
            self.previous.copy_from_slice(&self.current);
        }

        Ok(output)
    }

    fn emit_current(&self, output: &mut Vec<i16>) {
        output.extend(
            self.current
                .iter()
                .map(|sample| normalized_to_i16(f64::from(*sample))),
        );
    }
}

fn remix_frame<T>(input: &[T], output: &mut [f32])
where
    T: Copy,
    f32: FromSample<T>,
{
    if output.len() == input.len() {
        for (output, input) in output.iter_mut().zip(input) {
            *output = f32::from_sample(*input);
        }
    } else if output.len() == 1 {
        let sum = input
            .iter()
            .map(|sample| f64::from(f32::from_sample(*sample)))
            .sum::<f64>();
        output[0] = (sum / input.len() as f64) as f32;
    } else if input.len() == 1 {
        output.fill(f32::from_sample(input[0]));
    } else {
        // Preserve the complete input sound field when reducing to more than
        // one channel. Every input channel contributes to exactly one output
        // bucket, while expanding repeats the nearest input channel.
        let output_channels = output.len();
        for (output_index, sample) in output.iter_mut().enumerate() {
            let start = output_index.saturating_mul(input.len()) / output_channels;
            let end = (output_index + 1)
                .saturating_mul(input.len())
                .div_ceil(output_channels)
                .max(start + 1)
                .min(input.len());
            let sum = input[start..end]
                .iter()
                .map(|sample| f64::from(f32::from_sample(*sample)))
                .sum::<f64>();
            *sample = (sum / (end - start) as f64) as f32;
        }
    }
}

fn normalized_to_i16(sample: f64) -> i16 {
    let sample = sample.clamp(-1.0, 1.0);
    let scale = if sample < 0.0 { 32_768.0 } else { 32_767.0 };
    (sample * scale).round() as i16
}

#[cfg(test)]
mod tests {
    use super::*;
    use cpal::SupportedBufferSize;

    fn range(
        channels: u16,
        min_rate: u32,
        max_rate: u32,
        format: SampleFormat,
    ) -> SupportedStreamConfigRange {
        SupportedStreamConfigRange::new(
            channels,
            SampleRate(min_rate),
            SampleRate(max_rate),
            SupportedBufferSize::Unknown,
            format,
        )
    }

    #[test]
    fn device_selection_prefers_an_exact_requested_mode() {
        let selected = select_input_config(
            [
                range(2, 48_000, 48_000, SampleFormat::F32),
                range(1, 16_000, 16_000, SampleFormat::I16),
                range(1, 8_000, 96_000, SampleFormat::F32),
            ],
            16_000,
            1,
        )
        .unwrap();

        assert_eq!(selected.channels(), 1);
        assert_eq!(selected.sample_rate().0, 16_000);
        assert_eq!(selected.sample_format(), SampleFormat::I16);
    }

    #[test]
    fn device_selection_falls_back_to_the_nearest_native_rate() {
        let selected = select_input_config(
            [
                range(2, 44_100, 44_100, SampleFormat::F32),
                range(2, 48_000, 48_000, SampleFormat::F32),
            ],
            16_000,
            1,
        )
        .unwrap();

        assert_eq!(selected.channels(), 2);
        assert_eq!(selected.sample_rate().0, 44_100);
    }

    #[test]
    fn stereo_float_input_is_downmixed_and_converted_to_pcm() {
        let mut adapter = AudioAdapter::new(16_000, 2, 16_000, 1).unwrap();
        let output = adapter.process(&[1.0_f32, -1.0, 0.5, 0.5]).unwrap();

        assert_eq!(output, [0, 16_384]);
    }

    #[test]
    fn unsigned_native_input_uses_the_correct_zero_point() {
        let mut adapter = AudioAdapter::new(16_000, 1, 16_000, 1).unwrap();
        let output = adapter.process(&[0_u16, 32_768, u16::MAX]).unwrap();

        assert_eq!(output[0], i16::MIN);
        assert_eq!(output[1], 0);
        assert!(output[2] >= 32_766);
    }

    #[test]
    fn resampling_keeps_one_clock_across_callback_boundaries() {
        let mut whole = AudioAdapter::new(48_000, 1, 16_000, 1).unwrap();
        let all = whole
            .process(&[0_i16, 3_000, 6_000, 9_000, 12_000, 15_000, 18_000])
            .unwrap();

        let mut split = AudioAdapter::new(48_000, 1, 16_000, 1).unwrap();
        let mut pieces = split.process(&[0_i16, 3_000]).unwrap();
        pieces.extend(split.process(&[6_000_i16, 9_000, 12_000]).unwrap());
        pieces.extend(split.process(&[15_000_i16, 18_000]).unwrap());

        assert_eq!(pieces, all);
        assert_eq!(all[0], 0);
        assert!(all[1].abs_diff(9_000) <= 1);
        assert!(all[2].abs_diff(18_000) <= 1);
    }

    #[test]
    fn upsampling_interpolates_instead_of_repeating_blocks() {
        let mut adapter = AudioAdapter::new(2, 1, 4, 1).unwrap();
        let output = adapter.process(&[0_i16, 10_000, 20_000]).unwrap();

        assert_eq!(output[0], 0);
        for (actual, expected) in output[1..].iter().zip([5_000_i16, 10_000, 15_000, 20_000]) {
            assert!(actual.abs_diff(expected) <= 1, "{actual} != {expected}");
        }
    }

    #[test]
    fn malformed_interleaved_callbacks_are_rejected() {
        let mut adapter = AudioAdapter::new(48_000, 2, 16_000, 1).unwrap();
        let error = adapter.process(&[1_i16, 2, 3]).unwrap_err();

        assert!(error.contains("whole number"), "{error}");
    }
}

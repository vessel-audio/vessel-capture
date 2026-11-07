//! Input stream configuration negotiation against a device's supported ranges.

use cpal::traits::DeviceTrait;

use crate::error::CaptureError;

const PREFERRED_SAMPLE_RATE: u32 = 48_000;
const PREFERRED_BUFFER_SIZE: u32 = 256;

/// Configuration for an input capture stream.
///
/// Only mono (1 channel) and stereo (2 channels) are supported.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InputConfig {
    pub sample_rate: f64,
    pub channels: u16,
    pub frames_per_buffer: u32,
}

impl InputConfig {
    pub fn mono(sample_rate: f64, frames_per_buffer: u32) -> Result<Self, CaptureError> {
        Self::new(sample_rate, 1, frames_per_buffer)
    }

    pub fn stereo(sample_rate: f64, frames_per_buffer: u32) -> Result<Self, CaptureError> {
        Self::new(sample_rate, 2, frames_per_buffer)
    }

    pub fn new(
        sample_rate: f64,
        channels: u16,
        frames_per_buffer: u32,
    ) -> Result<Self, CaptureError> {
        if !sample_rate.is_finite() || sample_rate <= 0.0 || sample_rate.fract() != 0.0 {
            return Err(CaptureError::InvalidSampleRate(sample_rate));
        }
        if channels != 1 && channels != 2 {
            return Err(CaptureError::InvalidInputChannelCount(channels));
        }
        if frames_per_buffer == 0 {
            return Err(CaptureError::Capture(
                "frames per buffer must be positive".into(),
            ));
        }
        Ok(Self {
            sample_rate,
            channels,
            frames_per_buffer,
        })
    }
}

/// Negotiate an input config from the device's supported input configurations.
///
/// Prefers f32 samples, stereo channels, 48 kHz sample rate, and 256-frame
/// buffer size, falling back to the closest supported values. Returns
/// `NoSupportedInputConfig` when no f32 input format is available.
pub fn negotiate_input_config(device: &cpal::Device) -> Result<InputConfig, CaptureError> {
    let supported: Vec<cpal::SupportedStreamConfigRange> = device
        .supported_input_configs()
        .map_err(|e| CaptureError::InputSupportedConfigs(e.to_string()))?
        .collect();

    select_input_config_from_ranges(&supported)
}

/// Select the best `InputConfig` from a list of supported stream config ranges.
///
/// Prefers f32 samples, stereo channels, 48 kHz sample rate, and 256-frame
/// buffer size, falling back to the closest supported values. All config
/// properties are derived from a single selected range. When multiple ranges
/// match, stereo beats mono, 48 kHz support beats fallback, and 256-frame
/// buffer support beats fallback.
///
/// Returns `NoSupportedInputConfig` when no f32 mono/stereo range is present.
/// This function is pure (no hardware access) so it can be unit tested.
pub fn select_input_config_from_ranges(
    supported: &[cpal::SupportedStreamConfigRange],
) -> Result<InputConfig, CaptureError> {
    if supported.is_empty() {
        return Err(CaptureError::NoSupportedInputConfig);
    }

    // Only f32 mono or stereo input is supported. Devices with only
    // >2 channel f32 input are explicitly rejected.
    let f32_mono_stereo: Vec<&cpal::SupportedStreamConfigRange> = supported
        .iter()
        .filter(|c| {
            c.sample_format() == cpal::SampleFormat::F32 && (c.channels() == 1 || c.channels() == 2)
        })
        .collect();

    if f32_mono_stereo.is_empty() {
        return Err(CaptureError::NoSupportedInputConfig);
    }

    // Score each range and pick the best one. Score priorities:
    // 1. Stereo over mono
    // 2. Closest sample rate to 48 kHz (0 = within range, negative = distance)
    // 3. Closest buffer size to 256 frames (0 = within range, negative = distance,
    //    very negative for Unknown)
    //
    // Using signed distance ensures the closest fallback range wins when
    // no range exactly supports the preferred values.
    let best = f32_mono_stereo
        .iter()
        .max_by_key(|r| {
            let stereo_score: i32 = if r.channels() == 2 { 1 } else { 0 };

            let rate_score: i32 = {
                let min = r.min_sample_rate();
                let max = r.max_sample_rate();
                if min <= PREFERRED_SAMPLE_RATE && max >= PREFERRED_SAMPLE_RATE {
                    0
                } else if max < PREFERRED_SAMPLE_RATE {
                    -((PREFERRED_SAMPLE_RATE - max) as i32)
                } else {
                    -((min - PREFERRED_SAMPLE_RATE) as i32)
                }
            };

            let buf_score: i32 = match r.buffer_size() {
                cpal::SupportedBufferSize::Range { min, max } => {
                    if *min <= PREFERRED_BUFFER_SIZE && *max >= PREFERRED_BUFFER_SIZE {
                        0
                    } else if *max < PREFERRED_BUFFER_SIZE {
                        -((PREFERRED_BUFFER_SIZE - *max) as i32)
                    } else {
                        -((*min - PREFERRED_BUFFER_SIZE) as i32)
                    }
                }
                cpal::SupportedBufferSize::Unknown => i32::MIN / 2,
            };
            (stereo_score, rate_score, buf_score)
        })
        .expect("f32_mono_stereo is non-empty");

    let channels: u16 = if best.channels() == 2 { 2 } else { 1 };
    let sample_rate = PREFERRED_SAMPLE_RATE.clamp(best.min_sample_rate(), best.max_sample_rate());
    let frames_per_buffer = match best.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } => PREFERRED_BUFFER_SIZE.clamp(*min, *max),
        cpal::SupportedBufferSize::Unknown => 2048, // Conservative; host may deliver larger default buffers
    };

    InputConfig::new(sample_rate as f64, channels, frames_per_buffer)
}

/// Check whether an `InputConfig` falls within a `SupportedStreamConfigRange`.
///
/// Requires exact channel match (mono ↔ range with 1 channel, stereo ↔ range
/// with 2 channels), f32 sample format, sample rate within range, and buffer
/// size within range. A device with only 8-channel f32 ranges does NOT support
/// a 2-channel config via this predicate.
///
/// This is a pure function (no hardware access) so it can be unit tested.
pub fn input_config_matches_range(
    config: &InputConfig,
    range: &cpal::SupportedStreamConfigRange,
) -> bool {
    if range.sample_format() != cpal::SampleFormat::F32
        || range.channels() != config.channels
        || (config.sample_rate as u32) < range.min_sample_rate()
        || (config.sample_rate as u32) > range.max_sample_rate()
    {
        return false;
    }
    match range.buffer_size() {
        cpal::SupportedBufferSize::Range { min, max } => {
            config.frames_per_buffer >= *min && config.frames_per_buffer <= *max
        }
        cpal::SupportedBufferSize::Unknown => true,
    }
}

#[cfg(test)]
mod tests {
    use super::{input_config_matches_range, select_input_config_from_ranges, InputConfig};
    use crate::CaptureError;
    use cpal::{SampleFormat, SupportedBufferSize, SupportedStreamConfigRange};

    fn make_range(
        channels: u16,
        min_rate: u32,
        max_rate: u32,
        min_buf: u32,
        max_buf: u32,
        format: SampleFormat,
    ) -> SupportedStreamConfigRange {
        SupportedStreamConfigRange::new(
            channels,
            min_rate,
            max_rate,
            SupportedBufferSize::Range {
                min: min_buf,
                max: max_buf,
            },
            format,
        )
    }

    #[test]
    fn negotiate_prefers_stereo_over_mono() {
        let mono = make_range(1, 44_100, 48_000, 64, 1024, SampleFormat::F32);
        let stereo = make_range(2, 44_100, 48_000, 64, 1024, SampleFormat::F32);
        let selected = select_input_config_from_ranges(&[mono, stereo]).unwrap();
        assert_eq!(selected.channels, 2);
    }

    #[test]
    fn negotiate_falls_back_to_mono_when_no_stereo() {
        let mono = make_range(1, 44_100, 48_000, 64, 1024, SampleFormat::F32);
        let selected = select_input_config_from_ranges(&[mono]).unwrap();
        assert_eq!(selected.channels, 1);
    }

    #[test]
    fn negotiate_prefers_48k_over_fallback() {
        let low = make_range(2, 8_000, 44_100, 64, 1024, SampleFormat::F32);
        let full = make_range(2, 44_100, 48_000, 64, 1024, SampleFormat::F32);
        let selected = select_input_config_from_ranges(&[low, full]).unwrap();
        assert_eq!(selected.sample_rate, 48_000.0);
    }

    #[test]
    fn negotiate_clamps_sample_rate_to_selected_range() {
        let range = make_range(2, 44_100, 44_100, 64, 1024, SampleFormat::F32);
        let selected = select_input_config_from_ranges(&[range]).unwrap();
        assert_eq!(selected.sample_rate, 44_100.0);
    }

    #[test]
    fn negotiate_rejects_eight_channel_f32_only() {
        let eight = make_range(8, 44_100, 48_000, 64, 1024, SampleFormat::F32);
        let err = select_input_config_from_ranges(&[eight]).unwrap_err();
        assert!(matches!(err, CaptureError::NoSupportedInputConfig));
    }

    #[test]
    fn negotiate_rejects_non_f32_only() {
        let i16_stereo = make_range(2, 44_100, 48_000, 64, 1024, SampleFormat::I16);
        let err = select_input_config_from_ranges(&[i16_stereo]).unwrap_err();
        assert!(matches!(err, CaptureError::NoSupportedInputConfig));
    }

    #[test]
    fn negotiate_prefers_256_buffer_range() {
        let big = make_range(2, 44_100, 48_000, 512, 2048, SampleFormat::F32);
        let good = make_range(2, 44_100, 48_000, 64, 1024, SampleFormat::F32);
        let selected = select_input_config_from_ranges(&[big, good]).unwrap();
        assert_eq!(selected.frames_per_buffer, 256);
    }

    #[test]
    fn negotiate_falls_back_to_range_min_buffer_when_256_out_of_range() {
        let range = make_range(2, 44_100, 48_000, 512, 2048, SampleFormat::F32);
        let selected = select_input_config_from_ranges(&[range]).unwrap();
        assert_eq!(selected.frames_per_buffer, 512);
    }

    #[test]
    fn negotiate_returns_no_config_for_empty_list() {
        let err = select_input_config_from_ranges(&[]).unwrap_err();
        assert!(matches!(err, CaptureError::NoSupportedInputConfig));
    }

    #[test]
    fn matches_exact_mono_support() {
        let mono_range = make_range(1, 8000, 96000, 64, 4096, SampleFormat::F32);
        let mono_config = InputConfig::mono(48_000.0, 256).unwrap();
        assert!(input_config_matches_range(&mono_config, &mono_range));
    }

    #[test]
    fn matches_exact_stereo_support() {
        let config = InputConfig::stereo(48_000.0, 256).unwrap();
        let range = make_range(2, 8000, 96000, 64, 4096, SampleFormat::F32);
        assert!(input_config_matches_range(&config, &range));
    }

    #[test]
    fn eight_channel_range_does_not_support_stereo() {
        let config = InputConfig::stereo(48_000.0, 256).unwrap();
        let range = make_range(8, 8000, 96000, 64, 4096, SampleFormat::F32);
        assert!(!input_config_matches_range(&config, &range));
    }

    #[test]
    fn non_f32_range_does_not_support_mono() {
        let config = InputConfig::mono(48_000.0, 256).unwrap();
        let range = make_range(1, 8000, 96000, 64, 4096, SampleFormat::I16);
        assert!(!input_config_matches_range(&config, &range));
    }

    #[test]
    fn sample_rate_out_of_range_is_rejected() {
        let config = InputConfig::mono(48_000.0, 256).unwrap();
        let range = make_range(1, 8000, 16000, 64, 4096, SampleFormat::F32);
        assert!(!input_config_matches_range(&config, &range));
    }

    #[test]
    fn buffer_size_out_of_range_is_rejected() {
        let config = InputConfig::mono(48_000.0, 256).unwrap();
        let range = make_range(1, 8000, 96000, 512, 1024, SampleFormat::F32);
        assert!(!input_config_matches_range(&config, &range));
    }

    #[test]
    fn mono_config_accepted() {
        let config = InputConfig::mono(48_000.0, 256).unwrap();
        assert_eq!(config.sample_rate, 48_000.0);
        assert_eq!(config.channels, 1);
    }

    #[test]
    fn stereo_config_accepted() {
        let config = InputConfig::stereo(48_000.0, 256).unwrap();
        assert_eq!(config.channels, 2);
    }

    #[test]
    fn zero_channel_config_rejected() {
        let err = InputConfig::new(48_000.0, 0, 256).unwrap_err();
        assert!(matches!(err, CaptureError::InvalidInputChannelCount(0)));
    }

    #[test]
    fn more_than_two_channel_config_rejected() {
        let err = InputConfig::new(48_000.0, 3, 256).unwrap_err();
        assert!(matches!(err, CaptureError::InvalidInputChannelCount(3)));
    }

    #[test]
    fn invalid_sample_rate_rejected_for_input_config() {
        let err = InputConfig::new(0.0, 1, 256).unwrap_err();
        assert!(matches!(err, CaptureError::InvalidSampleRate(rate) if rate == 0.0));
    }
}

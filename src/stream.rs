//! Building the cpal input stream that feeds a [`CaptureHandle`].

use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{BufferSize, InputCallbackInfo, StreamConfig};

use crate::config::{input_config_matches_range, InputConfig};
use crate::error::CaptureError;
use crate::session::CaptureHandle;

/// Start capturing audio from an input device into a `CaptureHandle`.
///
/// Builds a cpal input stream with `f32` samples and starts it. The returned
/// stream must be kept alive for the duration of the capture; dropping it
/// stops recording.
///
/// The input callback feeds interleaved `f32` samples directly to
/// `handle.try_send_samples`. When the handle's stop token is set, the
/// callback silently drops incoming data, giving the consumer time to drain
/// buffered blocks before the channel closes.
pub fn start_input_stream(
    device: &cpal::Device,
    config: InputConfig,
    handle: &CaptureHandle,
) -> Result<cpal::Stream, CaptureError> {
    // Validate the requested config against the device's supported input ranges
    // using the same concrete-range semantics as select_input_config_from_ranges.
    let supported: Vec<cpal::SupportedStreamConfigRange> = device
        .supported_input_configs()
        .map_err(|e| CaptureError::InputSupportedConfigs(e.to_string()))?
        .collect();

    let matching_range = supported
        .iter()
        .find(|r| input_config_matches_range(&config, r))
        .ok_or(CaptureError::NoSupportedInputConfig)?;

    let buffer_size = match matching_range.buffer_size() {
        cpal::SupportedBufferSize::Unknown => BufferSize::Default,
        _ => BufferSize::Fixed(config.frames_per_buffer),
    };

    let stream_config = StreamConfig {
        channels: config.channels,
        sample_rate: config.sample_rate as u32,
        buffer_size,
    };

    let data_handle = handle.clone();
    let error_handle = handle.clone();
    let block_cap = handle.block_capacity();
    let stream = device
        .build_input_stream::<f32, _, _>(
            &stream_config,
            move |data: &[f32], _info: &InputCallbackInfo| {
                // Chunk into capacity-sized pieces so callbacks larger than
                // the preallocated blocks (possible with BufferSize::Default)
                // don't trigger a false terminal overflow.
                for chunk in data.chunks(block_cap) {
                    if data_handle.is_stopped() {
                        return;
                    }
                    if data_handle.try_send_samples(chunk).is_err() {
                        return;
                    }
                }
            },
            move |error| {
                eprintln!("audio input stream error: {error}");
                error_handle
                    .stream_error
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                error_handle
                    .stop_token
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            },
            None,
        )
        .map_err(CaptureError::InputBuildStream)?;

    stream.play().map_err(CaptureError::InputPlayStream)?;

    Ok(stream)
}

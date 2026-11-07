//! Microphone input capture on cpal (CoreAudio and friends): device
//! selection, config negotiation, and realtime-safe block delivery.
//!
//! Two consumption styles share one pipeline:
//! - [`CaptureSession`] (feature `wav-writer`): a writer thread finalizes a
//!   WAV file; overflow is terminal.
//! - [`block_pipeline`]: the caller drains f32 blocks and recycles them;
//!   pick [`OverflowPolicy::DropNewest`] for live feeds.

mod config;
mod device;
mod error;
mod session;
mod stream;
#[cfg(feature = "wav-writer")]
mod wav;

pub use config::{
    input_config_matches_range, negotiate_input_config, select_input_config_from_ranges,
    InputConfig,
};
pub use device::{
    list_input_devices, resolve_input_device, select_input_device, select_input_device_from_list,
    InputDeviceInfo,
};
pub use error::CaptureError;
#[cfg(feature = "wav-writer")]
pub use session::CaptureSession;
pub use session::{block_pipeline, CaptureHandle, OverflowPolicy};
pub use stream::start_input_stream;
#[cfg(feature = "wav-writer")]
pub use wav::StreamingWavWriter;

//! Error type shared by device selection, config negotiation, and capture.

use std::error::Error;
use std::fmt;

use cpal::{BuildStreamError, PlayStreamError};

#[derive(Debug)]
pub enum CaptureError {
    InvalidSampleRate(f64),
    InvalidFramesPerBuffer(u32),
    InvalidInputChannelCount(u16),
    NoDefaultInputDevice,
    InputDeviceNotFound(String),
    AmbiguousInputDevice(String),
    InputDeviceEnumeration(String),
    InputSupportedConfigs(String),
    NoSupportedInputConfig,
    InputBuildStream(BuildStreamError),
    InputPlayStream(PlayStreamError),
    CaptureQueueOverflow,
    Capture(String),
}

impl fmt::Display for CaptureError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSampleRate(sample_rate) => {
                write!(
                    f,
                    "sample rate must be finite and positive, got {sample_rate}"
                )
            }
            Self::InvalidFramesPerBuffer(frames_per_buffer) => {
                write!(
                    f,
                    "frames per buffer must be positive, got {frames_per_buffer}"
                )
            }
            Self::InvalidInputChannelCount(channels) => {
                write!(
                    f,
                    "only mono and stereo input are supported, got {channels} channels"
                )
            }
            Self::NoDefaultInputDevice => write!(f, "no default input device available"),
            Self::InputDeviceNotFound(name) => write!(f, "input device not found: {name}"),
            Self::AmbiguousInputDevice(name) => {
                write!(f, "multiple input devices named {name}")
            }
            Self::InputDeviceEnumeration(msg) => {
                write!(f, "failed to enumerate input devices: {msg}")
            }
            Self::InputSupportedConfigs(msg) => {
                write!(f, "failed to query supported input configs: {msg}")
            }
            Self::NoSupportedInputConfig => {
                write!(
                    f,
                    "no supported input configuration (need f32 mono or stereo)"
                )
            }
            Self::InputBuildStream(err) => write!(f, "failed to build input stream: {err}"),
            Self::InputPlayStream(err) => write!(f, "failed to start input stream: {err}"),
            Self::CaptureQueueOverflow => {
                write!(f, "capture queue overflow: writer thread fell behind")
            }
            Self::Capture(msg) => write!(f, "capture error: {msg}"),
        }
    }
}

impl Error for CaptureError {}

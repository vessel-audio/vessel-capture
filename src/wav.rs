//! Streaming 32-bit float WAV writer, used by the `wav-writer` feature.
//!
//! Thin layer over hound with the contract the capture session relies on:
//! misaligned chunks are rejected, the 4 GiB RIFF limit is enforced before
//! writing, finalizing zero frames is an error, and a writer that saw an
//! error removes its partial file.

use std::path::{Path, PathBuf};

use crate::error::CaptureError;

const BYTES_PER_SAMPLE: u64 = 4;

/// Streaming IEEE float WAV writer. Create, feed interleaved f32 chunks,
/// then [`finish`](Self::finish) to patch the headers.
pub struct StreamingWavWriter {
    // Option so finish() can take the writer out (hound finalizes by value).
    writer: Option<hound::WavWriter<std::io::BufWriter<std::fs::File>>>,
    channels: u16,
    total_samples: u64,
    errored: bool,
    path: PathBuf,
}

impl StreamingWavWriter {
    /// Create the file and write the header. `channels` must be 1 or 2 and
    /// `sample_rate` must be nonzero.
    pub fn create(
        path: impl AsRef<Path>,
        sample_rate: u32,
        channels: u16,
    ) -> Result<Self, CaptureError> {
        if sample_rate == 0 {
            return Err(CaptureError::Capture("sample rate must be positive".into()));
        }
        if channels == 0 || channels > 2 {
            return Err(CaptureError::Capture(format!(
                "unsupported channel count {channels} (only mono and stereo are supported)"
            )));
        }
        let path = path.as_ref().to_path_buf();
        let spec = hound::WavSpec {
            channels,
            sample_rate,
            bits_per_sample: 32,
            sample_format: hound::SampleFormat::Float,
        };
        let writer = hound::WavWriter::create(&path, spec)
            .map_err(|e| CaptureError::Capture(format!("failed to create WAV writer: {e}")))?;
        Ok(Self {
            writer: Some(writer),
            channels,
            total_samples: 0,
            errored: false,
            path,
        })
    }

    /// Write one chunk of interleaved samples; the length must be a multiple
    /// of the channel count and keep the file under the RIFF 4 GiB limit.
    pub fn write_interleaved(&mut self, interleaved: &[f32]) -> Result<(), CaptureError> {
        if self.errored {
            return Err(CaptureError::Capture("writer is in an error state".into()));
        }
        if interleaved.is_empty() {
            return Ok(());
        }
        if !interleaved.len().is_multiple_of(usize::from(self.channels)) {
            return Err(CaptureError::Capture(format!(
                "interleaved chunk length {} is not a multiple of channel count {}",
                interleaved.len(),
                self.channels
            )));
        }
        let data_size = (self.total_samples + interleaved.len() as u64) * BYTES_PER_SAMPLE;
        if data_size > u64::from(u32::MAX) - 36 {
            self.errored = true;
            return Err(CaptureError::Capture(format!(
                "audio data ({data_size} bytes) exceeds the RIFF/WAV 4 GiB limit"
            )));
        }
        let written = interleaved
            .iter()
            .try_for_each(|sample| self.writer.as_mut().unwrap().write_sample(*sample));
        if let Err(error) = written {
            self.errored = true;
            return Err(CaptureError::Capture(format!("WAV write failed: {error}")));
        }
        self.total_samples += interleaved.len() as u64;
        Ok(())
    }

    /// Patch the headers and return the frame count. Zero frames is an error
    /// (an empty file is not a valid take); any error removes the file.
    pub fn finish(mut self) -> Result<u64, CaptureError> {
        if self.errored {
            let _ = std::fs::remove_file(&self.path);
            return Err(CaptureError::Capture("writer is in an error state".into()));
        }
        let frames = self.total_samples / u64::from(self.channels);
        if frames == 0 {
            let _ = std::fs::remove_file(&self.path);
            return Err(CaptureError::Capture(
                "cannot finalize a WAV file with zero frames".into(),
            ));
        }
        self.writer
            .take()
            .unwrap()
            .finalize()
            .map_err(|e| CaptureError::Capture(format!("WAV finalize failed: {e}")))?;
        Ok(frames)
    }
}

impl Drop for StreamingWavWriter {
    fn drop(&mut self) {
        if self.errored {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::StreamingWavWriter;

    fn decode(path: &std::path::Path) -> (hound::WavSpec, Vec<f32>) {
        let mut reader = hound::WavReader::open(path).unwrap();
        let samples = reader
            .samples::<f32>()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        (reader.spec(), samples)
    }

    #[test]
    fn mono_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mono.wav");
        let mut writer = StreamingWavWriter::create(&path, 48_000, 1).unwrap();
        let samples: Vec<f32> = (0..256).map(|i| (i as f32 * 0.001).sin()).collect();
        writer.write_interleaved(&samples).unwrap();
        assert_eq!(writer.finish().unwrap(), 256);

        let (spec, decoded) = decode(&path);
        assert_eq!(spec.sample_rate, 48_000);
        assert_eq!(spec.channels, 1);
        assert_eq!(decoded.len(), 256);
        for (a, b) in decoded.iter().zip(samples.iter()) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn stereo_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stereo.wav");
        let mut writer = StreamingWavWriter::create(&path, 44_100, 2).unwrap();
        let interleaved: Vec<f32> = (0..128)
            .flat_map(|i| [(i as f32 * 0.01).sin(), (i as f32 * 0.01).cos()])
            .collect();
        writer.write_interleaved(&interleaved).unwrap();
        assert_eq!(writer.finish().unwrap(), 128);

        let (spec, decoded) = decode(&path);
        assert_eq!(spec.channels, 2);
        assert_eq!(decoded.len(), 256);
    }

    #[test]
    fn zero_frames_finish_fails_and_removes_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("zero.wav");
        let writer = StreamingWavWriter::create(&path, 48_000, 1).unwrap();
        assert!(writer.finish().is_err());
        assert!(!path.exists());
    }

    #[test]
    fn misaligned_chunk_rejected_without_erroring() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("misaligned.wav");
        let mut writer = StreamingWavWriter::create(&path, 48_000, 2).unwrap();
        assert!(writer.write_interleaved(&[1.0; 3]).is_err());
        // Not terminal: a valid write still lands.
        writer.write_interleaved(&[0.5; 4]).unwrap();
        assert_eq!(writer.finish().unwrap(), 2);
    }

    #[test]
    fn invalid_configs_rejected() {
        let dir = tempfile::tempdir().unwrap();
        assert!(StreamingWavWriter::create(dir.path().join("a.wav"), 0, 1).is_err());
        assert!(StreamingWavWriter::create(dir.path().join("b.wav"), 48_000, 0).is_err());
        assert!(StreamingWavWriter::create(dir.path().join("c.wav"), 48_000, 6).is_err());
    }

    #[test]
    fn missing_parent_directory_fails() {
        let path = std::path::Path::new("/nonexistent_dir_12345/test.wav");
        assert!(StreamingWavWriter::create(path, 48_000, 1).is_err());
    }
}

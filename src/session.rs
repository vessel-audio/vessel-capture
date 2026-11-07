//! The capture session: a realtime-safe block pipeline from the cpal input
//! callback to a consumer.
//!
//! The callback receives `&[f32]` blocks on a realtime thread where
//! allocation and blocking are forbidden. `CaptureHandle` copies each block
//! into a preallocated buffer from a lock-free free pool and ships it over a
//! bounded crossbeam channel. Who consumes the blocks depends on the mode:
//! a writer thread finalizing a WAV file ([`CaptureSession`]), or the
//! caller's own consumer draining a stream of blocks ([`BlockStream`]).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::config::InputConfig;
use crate::error::CaptureError;

/// How a full block queue is treated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverflowPolicy {
    /// Overflow is terminal: the error flag is set, stop is requested, and
    /// `join()` returns `CaptureQueueOverflow`. Right for recordings that
    /// must not silently lose audio.
    Fatal,
    /// Overflow drops the newest block and capture continues. Right for
    /// live feeds where a lagging consumer must not stall the microphone.
    DropNewest,
}

/// Handle for sending sample blocks from a realtime callback.
///
/// Uses preallocated sample blocks obtained from a lock-free free pool to
/// avoid allocation and locks on the realtime callback path.
/// Behaviour on a full queue depends on the [`OverflowPolicy`].
#[derive(Clone)]
pub struct CaptureHandle {
    data_tx: crossbeam_channel::Sender<Vec<f32>>,
    free_rx: crossbeam_channel::Receiver<Vec<f32>>,
    free_tx: crossbeam_channel::Sender<Vec<f32>>,
    pub(crate) stop_token: Arc<AtomicBool>,
    pub(crate) error_flag: Arc<AtomicBool>,
    pub(crate) stream_error: Arc<AtomicBool>,
    block_capacity: usize,
    overflow_policy: OverflowPolicy,
}

impl CaptureHandle {
    pub(crate) fn block_capacity(&self) -> usize {
        self.block_capacity
    }

    /// Try to send a block of interleaved samples without blocking or allocating.
    ///
    /// Copies `interleaved` into a preallocated block from the free pool.
    /// `interleaved.len()` must be ≤ the preallocated block capacity; a larger
    /// slice is always an error (with `Fatal` it is also terminal).
    pub fn try_send_samples(&self, interleaved: &[f32]) -> Result<(), CaptureError> {
        if self.error_flag.load(Ordering::Relaxed) {
            self.stop_token.store(true, Ordering::Relaxed);
            return Err(CaptureError::Capture("writer thread has failed".into()));
        }

        // Reject blocks that exceed the preallocated capacity before any copy.
        if interleaved.len() > self.block_capacity {
            return Err(self.overflow(CaptureError::CaptureQueueOverflow));
        }

        let mut block = match self.free_rx.try_recv() {
            Ok(b) => b,
            Err(_) => {
                // Free pool exhausted — the queue is full.
                return Err(self.overflow(CaptureError::CaptureQueueOverflow));
            }
        };
        block.clear();
        block.extend_from_slice(interleaved);
        match self.data_tx.try_send(block) {
            Ok(()) => Ok(()),
            Err(crossbeam_channel::TrySendError::Full(block)) => {
                let _ = self.free_tx.try_send(block);
                Err(self.overflow(CaptureError::CaptureQueueOverflow))
            }
            Err(crossbeam_channel::TrySendError::Disconnected(block)) => {
                let _ = self.free_tx.try_send(block);
                Err(self.overflow(CaptureError::Capture("writer channel disconnected".into())))
            }
        }
    }

    /// Apply the overflow policy: `Fatal` flags the error and requests stop;
    /// `DropNewest` just reports the dropped block.
    fn overflow(&self, err: CaptureError) -> CaptureError {
        match self.overflow_policy {
            OverflowPolicy::Fatal => self.set_error(err),
            OverflowPolicy::DropNewest => err,
        }
    }

    /// Set the error flag and request stop, returning the given error.
    fn set_error(&self, err: CaptureError) -> CaptureError {
        self.error_flag.store(true, Ordering::Relaxed);
        self.stop_token.store(true, Ordering::Relaxed);
        err
    }

    /// Request the writer thread to stop and finalize the WAV.
    pub fn request_stop(&self) {
        self.stop_token.store(true, Ordering::Relaxed);
    }

    /// Whether a stop has been requested.
    pub fn is_stopped(&self) -> bool {
        self.stop_token.load(Ordering::Relaxed)
    }

    /// Whether the writer thread or input stream has reported an error.
    pub fn has_error(&self) -> bool {
        self.error_flag.load(Ordering::Relaxed) || self.stream_error.load(Ordering::Relaxed)
    }

    /// Returns a reference to the stop token so callers can request stop
    /// without holding a full `CaptureHandle` clone (and its channel endpoints).
    pub fn stop_token(&self) -> &Arc<AtomicBool> {
        &self.stop_token
    }
}

/// The shared state behind every capture pipeline: flag set, block channels,
/// and the preallocated free pool.
struct Pipeline {
    handle: CaptureHandle,
    data_rx: crossbeam_channel::Receiver<Vec<f32>>,
}

fn build_pipeline(
    config: &InputConfig,
    queue_size: usize,
    policy: OverflowPolicy,
) -> Result<Pipeline, CaptureError> {
    if queue_size == 0 {
        return Err(CaptureError::Capture("queue size must be positive".into()));
    }

    let block_samples = config.frames_per_buffer as usize * config.channels as usize;

    let (data_tx, data_rx) = crossbeam_channel::bounded::<Vec<f32>>(queue_size);
    // Free pool has one extra block so the data channel can fill before
    // the pool is exhausted — this makes overflow detectable.
    let (free_tx, free_rx) = crossbeam_channel::bounded::<Vec<f32>>(queue_size + 1);

    // Preallocate all blocks and prime the free pool.
    for _ in 0..queue_size + 1 {
        free_tx
            .send(vec![0.0_f32; block_samples])
            .map_err(|_| CaptureError::Capture("failed to init pool".into()))?;
    }

    let stop_token = Arc::new(AtomicBool::new(false));
    let error_flag = Arc::new(AtomicBool::new(false));
    let stream_error = Arc::new(AtomicBool::new(false));

    let handle = CaptureHandle {
        data_tx,
        free_rx,
        free_tx,
        stop_token: stop_token.clone(),
        error_flag: error_flag.clone(),
        stream_error: stream_error.clone(),
        block_capacity: block_samples,
        overflow_policy: policy,
    };

    Ok(Pipeline { handle, data_rx })
}

/// A spawned writer thread that receives sample blocks and writes a WAV.
#[cfg(feature = "wav-writer")]
pub struct CaptureSession {
    handle: CaptureHandle,
    thread: Option<std::thread::JoinHandle<Result<u64, CaptureError>>>,
    temp_path: std::path::PathBuf,
    config: InputConfig,
    error_flag: Arc<AtomicBool>,
    stream_error: Arc<AtomicBool>,
}

#[cfg(feature = "wav-writer")]
impl CaptureSession {
    /// Spawn the writer thread and return a session.
    ///
    /// Preallocates `queue_size` sample blocks sized for one callback
    /// (`frames_per_buffer * channels`). The writer thread creates a
    /// `StreamingWavWriter` at `temp_path`, processes sample blocks,
    /// and returns blocks to the free pool for reuse by the callback.
    /// Both channels use lock-free crossbeam bounded channels to keep
    /// the callback path realtime-safe. Overflow is terminal (`Fatal`).
    pub fn spawn(
        temp_path: std::path::PathBuf,
        config: InputConfig,
        queue_size: usize,
    ) -> Result<Self, CaptureError> {
        let pipeline = build_pipeline(&config, queue_size, OverflowPolicy::Fatal)?;
        let Pipeline { handle, data_rx } = pipeline;

        let thread_error = handle.error_flag.clone();
        let thread_stop = handle.stop_token.clone();
        let writer_path = temp_path.clone();
        let sample_rate = config.sample_rate;
        let channels = config.channels;
        let free_tx = handle.free_tx.clone();

        let thread = std::thread::Builder::new()
            .name("capture-writer".into())
            .spawn(move || {
                wav_writer_thread(
                    data_rx,
                    free_tx,
                    &writer_path,
                    sample_rate,
                    channels,
                    thread_error,
                    thread_stop,
                )
            })
            .map_err(|e| CaptureError::Capture(format!("failed to spawn writer: {e}")))?;

        Ok(Self {
            error_flag: handle.error_flag.clone(),
            stream_error: handle.stream_error.clone(),
            handle,
            thread: Some(thread),
            temp_path,
            config,
        })
    }

    pub fn handle(&self) -> &CaptureHandle {
        &self.handle
    }

    pub fn config(&self) -> InputConfig {
        self.config
    }

    pub fn temp_path(&self) -> &std::path::Path {
        &self.temp_path
    }

    /// Stop the writer, finalize the WAV, and return the frame count.
    ///
    /// A flagged stream or overflow error surfaces here even when the writer
    /// itself finished cleanly; the temp file is removed in that case.
    pub fn join(mut self) -> Result<u64, CaptureError> {
        self.handle.request_stop();
        // Drop the handle (and its data_tx clone) so the writer thread
        // sees end-of-stream via a closed channel.
        drop(self.handle);

        match self.thread.take() {
            Some(thread) => match thread.join() {
                Ok(Ok(frames)) => {
                    if self.stream_error.load(Ordering::Relaxed) {
                        let _ = std::fs::remove_file(&self.temp_path);
                        return Err(CaptureError::Capture("audio input stream error".into()));
                    }
                    if self.error_flag.load(Ordering::Relaxed) {
                        let _ = std::fs::remove_file(&self.temp_path);
                        return Err(CaptureError::CaptureQueueOverflow);
                    }
                    Ok(frames)
                }
                Ok(Err(e)) => Err(e),
                Err(_) => {
                    let _ = std::fs::remove_file(&self.temp_path);
                    Err(CaptureError::Capture("writer thread panicked".into()))
                }
            },
            None => Err(CaptureError::Capture(
                "capture session already joined".into(),
            )),
        }
    }
}

/// A capture pipeline without a built-in consumer: the caller drains f32
/// blocks from the returned receiver and recycles them with
/// [`CaptureHandle::recycle`] so the pool does not run dry.
///
/// Well suited to live consumers (streaming ASR, meters) that do not want a
/// WAV file. Choose [`OverflowPolicy::DropNewest`] so a lagging consumer
/// loses audio instead of killing the take.
pub fn block_pipeline(
    config: &InputConfig,
    queue_size: usize,
    policy: OverflowPolicy,
) -> Result<(CaptureHandle, crossbeam_channel::Receiver<Vec<f32>>), CaptureError> {
    let pipeline = build_pipeline(config, queue_size, policy)?;
    Ok((pipeline.handle, pipeline.data_rx))
}

impl CaptureHandle {
    /// Return a consumed block to the free pool for reuse by the callback.
    pub fn recycle(&self, block: Vec<f32>) {
        let _ = self.free_tx.try_send(block);
    }
}

/// The WAV writer thread: receive blocks, write interleaved, recycle blocks.
/// On stop, drain remaining blocks so short recordings are not truncated.
#[cfg(feature = "wav-writer")]
fn wav_writer_thread(
    data_rx: crossbeam_channel::Receiver<Vec<f32>>,
    free_tx: crossbeam_channel::Sender<Vec<f32>>,
    path: &std::path::Path,
    sample_rate: f64,
    channels: u16,
    error_flag: Arc<AtomicBool>,
    stop_token: Arc<AtomicBool>,
) -> Result<u64, CaptureError> {
    let mut writer = crate::wav::StreamingWavWriter::create(path, sample_rate as u32, channels)
        .map_err(|e| {
            error_flag.store(true, Ordering::Relaxed);
            stop_token.store(true, Ordering::Relaxed);
            let _ = std::fs::remove_file(path);
            CaptureError::Capture(format!("failed to create WAV writer: {e}"))
        })?;

    let mut total_frames: u64 = 0;

    loop {
        match data_rx.recv_timeout(std::time::Duration::from_millis(500)) {
            Ok(data) => {
                let frame_count = data.len() as u64 / channels as u64;
                if let Err(e) = writer.write_interleaved(&data) {
                    error_flag.store(true, Ordering::Relaxed);
                    stop_token.store(true, Ordering::Relaxed);
                    drop(writer);
                    let _ = std::fs::remove_file(path);
                    return Err(CaptureError::Capture(format!("WAV write failed: {e}")));
                }
                total_frames += frame_count;
                // Return the block to the free pool. Use try_send so the writer
                // never blocks when the feed thread has already exited and nobody
                // is draining the free pool.
                let _ = free_tx.try_send(data);
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                if stop_token.load(Ordering::Relaxed) {
                    // Stop was requested — drain any remaining blocks before
                    // finalizing so short recordings aren't truncated.
                    while let Ok(data) = data_rx.try_recv() {
                        let frame_count = data.len() as u64 / channels as u64;
                        if let Err(e) = writer.write_interleaved(&data) {
                            error_flag.store(true, Ordering::Relaxed);
                            stop_token.store(true, Ordering::Relaxed);
                            drop(writer);
                            let _ = std::fs::remove_file(path);
                            return Err(CaptureError::Capture(format!(
                                "WAV write failed during drain: {e}"
                            )));
                        }
                        total_frames += frame_count;
                        let _ = free_tx.try_send(data);
                    }
                    break;
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }

    writer.finish().map_err(|e| {
        error_flag.store(true, Ordering::Relaxed);
        stop_token.store(true, Ordering::Relaxed);
        let _ = std::fs::remove_file(path);
        CaptureError::Capture(format!("WAV finalize failed: {e}"))
    })?;

    Ok(total_frames)
}

#[cfg(all(test, feature = "wav-writer"))]
mod tests {
    use super::{block_pipeline, CaptureSession, InputConfig, OverflowPolicy};
    use crate::CaptureError;

    fn decode(path: &std::path::Path) -> (hound::WavSpec, Vec<f32>) {
        let mut reader = hound::WavReader::open(path).unwrap();
        let samples = reader
            .samples::<f32>()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        (reader.spec(), samples)
    }

    fn make_capture_config() -> InputConfig {
        InputConfig::mono(48_000.0, 512).unwrap()
    }

    fn make_temp_path(dir: &std::path::Path) -> std::path::PathBuf {
        dir.join("capture.wav")
    }

    #[test]
    fn capture_fixed_duration_produces_expected_frame_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = make_temp_path(dir.path());
        let config = make_capture_config();

        let session = CaptureSession::spawn(path.clone(), config, 16).unwrap();
        let handle = session.handle().clone();

        // Send 480 samples (480 mono frames at 48kHz = 10ms)
        for _ in 0..10 {
            handle.try_send_samples(&[0.25; 48]).unwrap();
        }
        drop(handle);

        let frames = session.join().unwrap();
        assert_eq!(frames, 480);
        assert!(path.exists());

        let decoded = decode(&path);
        assert_eq!(decoded.1.len() as u64 / u64::from(decoded.0.channels), 480);
        assert_eq!(decoded.0.channels, 1);
    }

    #[test]
    fn capture_stereo_produces_correct_channels() {
        let dir = tempfile::tempdir().unwrap();
        let path = make_temp_path(dir.path());
        let config = InputConfig::stereo(48_000.0, 256).unwrap();

        let session = CaptureSession::spawn(path.clone(), config, 4).unwrap();
        let handle = session.handle().clone();

        // Stereo: 96 interleaved samples = 48 frames
        handle.try_send_samples(&[0.1; 96]).unwrap();
        drop(handle);

        let frames = session.join().unwrap();
        assert_eq!(frames, 48);

        let decoded = decode(&path);
        assert_eq!(decoded.0.channels, 2);
        assert_eq!(decoded.1.len() as u64 / u64::from(decoded.0.channels), 48);
    }

    #[test]
    fn stop_token_causes_graceful_finalize_after_queue_drain() {
        let dir = tempfile::tempdir().unwrap();
        let path = make_temp_path(dir.path());
        let config = make_capture_config();

        let session = CaptureSession::spawn(path.clone(), config, 16).unwrap();
        let handle = session.handle().clone();

        handle.try_send_samples(&[0.5; 100]).unwrap();
        handle.request_stop();
        // Stop flag tells the callback to stop sending, but already-buffered
        // samples are still drained and written by the writer thread.
        drop(handle);

        let frames = session.join().unwrap();
        assert_eq!(frames, 100);
        assert!(path.exists());
    }

    #[test]
    fn queue_overflow_fails_recording() {
        let dir = tempfile::tempdir().unwrap();
        let path = make_temp_path(dir.path());
        let config = make_capture_config();

        // The free pool holds one more block than the data channel capacity.
        // With queue_size=1 the first send fills the data channel; the second
        // send grabs the remaining pool block and hits TrySendError::Full.
        let session = CaptureSession::spawn(path.clone(), config, 1).unwrap();

        // The sender's lock-free channel ops always outpace the writer thread
        // which must perform file I/O for each block, so overflow is
        // guaranteed within a handful of sends.
        let err = loop {
            match session.handle().try_send_samples(&[0.0; 16]) {
                Ok(()) => continue,
                Err(e @ CaptureError::CaptureQueueOverflow) => break e,
                Err(e) => panic!("unexpected error: {e:?}"),
            }
        };

        assert!(matches!(err, CaptureError::CaptureQueueOverflow));

        // Overflow is terminal: join returns the overflow error and removes
        // the temp file.
        let join_err = session.join().unwrap_err();
        assert!(matches!(join_err, CaptureError::CaptureQueueOverflow));
        assert!(!path.exists());
    }

    #[test]
    fn zero_frame_stop_fails_and_removes_temp() {
        let dir = tempfile::tempdir().unwrap();
        let path = make_temp_path(dir.path());
        let config = make_capture_config();

        let session = CaptureSession::spawn(path.clone(), config, 4).unwrap();
        let handle = session.handle().clone();
        // Drop handle without sending any samples
        drop(handle);

        let err = session.join().unwrap_err();
        assert!(matches!(err, CaptureError::Capture(_)));
        // Temp file should not exist
        assert!(!path.exists());
    }

    #[test]
    fn overflow_sets_error_flag_and_stop_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = make_temp_path(dir.path());
        let config = make_capture_config();

        let session = CaptureSession::spawn(path.clone(), config, 1).unwrap();
        let handle = session.handle().clone();

        let err = loop {
            match handle.try_send_samples(&[0.0; 16]) {
                Ok(()) => continue,
                Err(e @ CaptureError::CaptureQueueOverflow) => break e,
                Err(e) => panic!("unexpected error: {e:?}"),
            }
        };

        assert!(matches!(err, CaptureError::CaptureQueueOverflow));
        assert!(handle.has_error());
        assert!(handle.is_stopped());
    }

    #[test]
    fn capture_roundtrip_through_decode() {
        let dir = tempfile::tempdir().unwrap();
        let path = make_temp_path(dir.path());
        let config = make_capture_config();

        let session = CaptureSession::spawn(path.clone(), config, 4).unwrap();
        let handle = session.handle().clone();

        // Send a sine wave: 480 frames
        let samples: Vec<f32> = (0..480).map(|i| (i as f32 * 0.01).sin()).collect();
        handle.try_send_samples(&samples).unwrap();
        drop(handle);

        let frames = session.join().unwrap();
        assert_eq!(frames, 480);

        let decoded = decode(&path);
        assert_eq!(decoded.1.len() as u64 / u64::from(decoded.0.channels), 480);
        assert_eq!(decoded.0.channels, 1);
        assert_eq!(decoded.0.sample_rate, 48_000);
        for (a, b) in decoded.1.as_slice().iter().zip(samples.iter()) {
            assert!((a - b).abs() < 1e-6);
        }
    }

    #[test]
    fn multi_block_capture_commits_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let path = make_temp_path(dir.path());
        let config = make_capture_config();

        let session = CaptureSession::spawn(path.clone(), config, 32).unwrap();
        let handle = session.handle().clone();

        let total_frames = 1024;
        let block_size = 128;
        for _ in 0..(total_frames / block_size) {
            let block = vec![0.25; block_size];
            handle.try_send_samples(&block).unwrap();
        }
        drop(handle);

        let frames = session.join().unwrap();
        assert_eq!(frames, total_frames as u64);

        let decoded = decode(&path);
        assert_eq!(
            decoded.1.len() as u64 / u64::from(decoded.0.channels),
            total_frames as u64
        );
    }

    #[test]
    fn drop_newest_overflow_is_not_terminal() {
        let config = make_capture_config();
        let (handle, _blocks) = block_pipeline(&config, 1, OverflowPolicy::DropNewest).unwrap();

        // Overflow the tiny queue; with DropNewest the error reports but the
        // pipeline keeps accepting blocks afterwards.
        let mut overflowed = false;
        for _ in 0..16 {
            match handle.try_send_samples(&[0.0; 16]) {
                Ok(()) => {}
                Err(CaptureError::CaptureQueueOverflow) => overflowed = true,
                Err(e) => panic!("unexpected error: {e:?}"),
            }
        }
        assert!(overflowed);
        assert!(!handle.has_error());
        assert!(!handle.is_stopped());
    }
}

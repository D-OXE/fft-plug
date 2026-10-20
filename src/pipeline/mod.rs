use std::cell::UnsafeCell;
use std::fmt;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle, Thread};

use crate::dsp::{Analyzer, DspConfig, DspError, SpectrumSnapshot};

pub const PCM_RING_SLOTS: usize = 8;
pub const MAX_PCM_FRAMES: usize = 8192;

const SLOT_FREE: u8 = 0;
const SLOT_WRITING: u8 = 1;
const SLOT_READY: u8 = 2;
const SLOT_READING: u8 = 3;
const QUIET_THRESHOLD: f32 = 0.0008;
const WAKE_THRESHOLD: f32 = 0.0012;
const DECAY_START_SECONDS: f32 = 0.12;
const SLEEP_AFTER_SECONDS: f32 = 0.55;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ActivityState {
    Active = 0,
    Decaying = 1,
    Sleeping = 2,
}

impl ActivityState {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::Decaying,
            2 => Self::Sleeping,
            _ => Self::Active,
        }
    }
}

#[derive(Debug)]
pub enum PipelineError {
    InvalidRing(String),
    Dsp(DspError),
    Worker(io::Error),
}

impl fmt::Display for PipelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRing(message) => write!(f, "invalid PCM ring: {message}"),
            Self::Dsp(error) => write!(f, "DSP initialization failed: {error}"),
            Self::Worker(error) => write!(f, "DSP worker could not start: {error}"),
        }
    }
}

impl std::error::Error for PipelineError {}

impl From<DspError> for PipelineError {
    fn from(error: DspError) -> Self {
        Self::Dsp(error)
    }
}

#[derive(Debug)]
pub struct AudioBlock {
    samples: Vec<f32>,
    frames: usize,
    sample_rate: u32,
}

impl AudioBlock {
    fn new(max_frames: usize) -> Self {
        Self {
            samples: vec![0.0; max_frames],
            frames: 0,
            sample_rate: 0,
        }
    }

    pub fn samples(&self) -> &[f32] {
        &self.samples[..self.frames]
    }

    pub fn frames(&self) -> usize {
        self.frames
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn samples_mut(&mut self) -> &mut [f32] {
        &mut self.samples
    }

    pub fn set_format(&mut self, frames: usize, sample_rate: u32) {
        self.frames = frames.min(self.samples.len());
        self.sample_rate = sample_rate;
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RingStats {
    pub overwritten_blocks: u64,
    pub rejected_blocks: u64,
    pub published_frames: u64,
    pub consumed_frames: u64,
    pub queued_blocks: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PipelineStats {
    pub ring: RingStats,
    pub dsp_frames: u64,
    pub last_peak_hz: f32,
    pub last_peak_level: f32,
    pub activity: ActivityState,
}

struct PipelineMetrics {
    dsp_frames: AtomicU64,
    last_peak_hz_bits: AtomicU32,
    last_peak_level_bits: AtomicU32,
}

impl PipelineMetrics {
    fn new() -> Self {
        Self {
            dsp_frames: AtomicU64::new(0),
            last_peak_hz_bits: AtomicU32::new(0.0_f32.to_bits()),
            last_peak_level_bits: AtomicU32::new(0.0_f32.to_bits()),
        }
    }
}

struct PcmSlot {
    block: UnsafeCell<AudioBlock>,
    state: AtomicU8,
}

/// 固定槽位的单生产者/单消费者有序 PCM FIFO。
///
/// 生产者按 `write_cursor` 写入，消费者按 `read_cursor` 读取，槽位状态负责交接所有权。
/// FIFO 满时优先回收最旧的 READY 槽位；如果最旧槽位正在被消费者读取，则丢弃当前输入，
/// 两种情况都会记录 overrun 并标记 PCM discontinuity。这样 DSP 不会把断点两侧的样本
/// 默默拼接到同一个 FFT window 中。
pub struct PcmRing {
    slots: Box<[PcmSlot]>,
    max_frames: usize,
    read_cursor: AtomicU64,
    write_cursor: AtomicU64,
    overwritten_blocks: AtomicU64,
    rejected_blocks: AtomicU64,
    published_frames: AtomicU64,
    consumed_frames: AtomicU64,
    discontinuity: AtomicBool,
    worker_waker: OnceLock<Thread>,
    wake_pending: AtomicBool,
}

// SAFETY: 槽位在构造后不会增删。唯一生产者只写通过 CAS 获得的 WRITING 槽位；
// 唯一消费者只读通过 CAS 获得的 READING 槽位。每个槽位的原子状态完成所有权交接，
// 不会在消费者仍持有 READING 槽位时覆盖其 AudioBlock。
unsafe impl Send for PcmRing {}
// SAFETY: 同上；Sync 只允许这套明确的单生产者/单消费者协议跨线程访问。
unsafe impl Sync for PcmRing {}

impl PcmRing {
    pub fn new(max_frames: usize) -> Result<Self, PipelineError> {
        if max_frames == 0 {
            return Err(PipelineError::InvalidRing(
                "maximum frame count must be greater than zero".to_string(),
            ));
        }

        let slots = (0..PCM_RING_SLOTS)
            .map(|_| PcmSlot {
                block: UnsafeCell::new(AudioBlock::new(max_frames)),
                state: AtomicU8::new(SLOT_FREE),
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();

        Ok(Self {
            slots,
            max_frames,
            read_cursor: AtomicU64::new(0),
            write_cursor: AtomicU64::new(0),
            overwritten_blocks: AtomicU64::new(0),
            rejected_blocks: AtomicU64::new(0),
            published_frames: AtomicU64::new(0),
            consumed_frames: AtomicU64::new(0),
            discontinuity: AtomicBool::new(false),
            worker_waker: OnceLock::new(),
            wake_pending: AtomicBool::new(false),
        })
    }

    /// 将一个 PCM block 按顺序写入 FIFO。
    ///
    /// `sample_at` 在 callback 中直接解码样本，不要求创建临时 Vec。若输入超过槽位容量，
    /// 只保留尾部样本并标记 discontinuity，以免把截断前后的数据当成连续时域输入。
    pub fn push_with<F>(&self, frame_count: usize, sample_rate: u32, mut sample_at: F) -> bool
    where
        F: FnMut(usize) -> f32,
    {
        if frame_count == 0 || sample_rate == 0 {
            return false;
        }

        let write_cursor = self.write_cursor.load(Ordering::Relaxed);
        let Some(slot) = self.claim_write_slot(write_cursor) else {
            self.rejected_blocks.fetch_add(1, Ordering::Relaxed);
            self.discontinuity.store(true, Ordering::Release);
            return false;
        };

        let stored_frames = frame_count.min(self.max_frames);
        let source_start = frame_count - stored_frames;

        // 生产者在发布前独占这个槽位，填充过程不涉及锁或分配。
        // SAFETY: claim_write_slot 只返回本次获得 SLOT_WRITING 的槽位，消费者不会读取写入中
        // 的槽位。
        unsafe {
            let block = &mut *slot.block.get();
            for destination in 0..stored_frames {
                block.samples_mut()[destination] = sample_at(source_start + destination);
            }
            block.set_format(stored_frames, sample_rate);
        }

        slot.state.store(SLOT_READY, Ordering::Release);
        self.write_cursor
            .store(write_cursor.wrapping_add(1), Ordering::Release);
        self.published_frames
            .fetch_add(stored_frames as u64, Ordering::Relaxed);
        if source_start > 0 {
            self.overwritten_blocks.fetch_add(1, Ordering::Relaxed);
            self.discontinuity.store(true, Ordering::Release);
        }
        self.notify_worker();
        true
    }

    /// Register the single DSP consumer after its thread has been created.
    ///
    /// The callback path only reads this lock-free `OnceLock` and calls `unpark`; it never takes
    /// a mutex or allocates. Registration also handles input that arrived during thread startup.
    fn register_worker(&self, worker: Thread) {
        let _ = self.worker_waker.set(worker);
        if self.has_ready_block() {
            self.notify_worker();
        }
    }

    fn notify_worker(&self) {
        let Some(worker) = self.worker_waker.get() else {
            return;
        };
        if !self.wake_pending.swap(true, Ordering::AcqRel) {
            worker.unpark();
        }
    }

    fn wake_worker(&self) {
        let Some(worker) = self.worker_waker.get() else {
            return;
        };
        self.wake_pending.store(true, Ordering::Release);
        worker.unpark();
    }

    fn prepare_to_park(&self) -> bool {
        self.wake_pending.store(false, Ordering::Release);
        self.has_ready_block()
    }

    fn has_ready_block(&self) -> bool {
        let read_cursor = self.read_cursor.load(Ordering::Acquire);
        self.slots[(read_cursor as usize) % self.slots.len()]
            .state
            .load(Ordering::Acquire)
            == SLOT_READY
    }

    fn claim_write_slot(&self, write_cursor: u64) -> Option<&PcmSlot> {
        loop {
            let slot = &self.slots[(write_cursor as usize) % self.slots.len()];
            if slot
                .state
                .compare_exchange(SLOT_FREE, SLOT_WRITING, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(slot);
            }

            let read_cursor = self.read_cursor.load(Ordering::Acquire);
            let oldest = &self.slots[(read_cursor as usize) % self.slots.len()];
            if oldest
                .state
                .compare_exchange(SLOT_READY, SLOT_FREE, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                self.read_cursor
                    .store(read_cursor.wrapping_add(1), Ordering::Release);
                self.overwritten_blocks.fetch_add(1, Ordering::Relaxed);
                self.discontinuity.store(true, Ordering::Release);
                continue;
            }

            return None;
        }
    }

    pub fn with_next<F, R>(&self, read: F) -> Option<R>
    where
        F: FnOnce(&AudioBlock) -> R,
    {
        let read_cursor = self.read_cursor.load(Ordering::Relaxed);
        let slot = &self.slots[(read_cursor as usize) % self.slots.len()];
        if slot
            .state
            .compare_exchange(
                SLOT_READY,
                SLOT_READING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_err()
        {
            return None;
        }

        // SAFETY: SLOT_READY -> SLOT_READING 已交接给本次唯一消费者，生产者不会覆盖该槽位。
        let (result, frames) = unsafe {
            let block = &*slot.block.get();
            (read(block), block.frames())
        };
        slot.state.store(SLOT_FREE, Ordering::Release);
        self.read_cursor
            .store(read_cursor.wrapping_add(1), Ordering::Release);
        self.consumed_frames
            .fetch_add(frames as u64, Ordering::Relaxed);
        Some(result)
    }

    /// 保留旧入口以减少调用方破坏；FIFO 语义下它读取下一个有序 block，而不是跳到最新 block。
    pub fn with_latest<F, R>(&self, read: F) -> Option<R>
    where
        F: FnOnce(&AudioBlock) -> R,
    {
        self.with_next(read)
    }

    fn take_discontinuity(&self) -> bool {
        self.discontinuity.swap(false, Ordering::AcqRel)
    }

    pub fn stats(&self) -> RingStats {
        let read_cursor = self.read_cursor.load(Ordering::Acquire);
        let write_cursor = self.write_cursor.load(Ordering::Acquire);
        RingStats {
            overwritten_blocks: self.overwritten_blocks.load(Ordering::Relaxed),
            rejected_blocks: self.rejected_blocks.load(Ordering::Relaxed),
            published_frames: self.published_frames.load(Ordering::Relaxed),
            consumed_frames: self.consumed_frames.load(Ordering::Relaxed),
            queued_blocks: write_cursor.saturating_sub(read_cursor),
        }
    }
}

pub struct SnapshotStore {
    latest: Mutex<Option<Arc<SpectrumSnapshot>>>,
}

impl SnapshotStore {
    pub fn new() -> Self {
        Self {
            latest: Mutex::new(None),
        }
    }

    pub fn publish(&self, snapshot: SpectrumSnapshot) {
        if let Ok(mut latest) = self.latest.lock() {
            *latest = Some(Arc::new(snapshot));
        }
    }

    pub fn latest(&self) -> Option<Arc<SpectrumSnapshot>> {
        self.latest
            .lock()
            .ok()
            .and_then(|latest| latest.as_ref().cloned())
    }
}

impl Default for SnapshotStore {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ActivitySignal {
    state: AtomicU8,
    wake_requested: AtomicBool,
}

impl ActivitySignal {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(ActivityState::Sleeping as u8),
            wake_requested: AtomicBool::new(false),
        }
    }

    fn set_state(&self, state: ActivityState) {
        self.state.store(state as u8, Ordering::Release);
    }

    fn request_wake(&self) {
        self.wake_requested.store(true, Ordering::Release);
    }

    pub fn state(&self) -> ActivityState {
        ActivityState::from_u8(self.state.load(Ordering::Acquire))
    }

    pub fn take_wake_request(&self) -> bool {
        self.wake_requested.swap(false, Ordering::AcqRel)
    }
}

pub struct Pipeline {
    ring: Arc<PcmRing>,
    snapshots: Arc<SnapshotStore>,
    activity: Arc<ActivitySignal>,
    stop: Arc<AtomicBool>,
    metrics: Arc<PipelineMetrics>,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl Pipeline {
    pub fn start(config: DspConfig) -> Result<Arc<Self>, PipelineError> {
        let ring = Arc::new(PcmRing::new(MAX_PCM_FRAMES)?);
        let snapshots = Arc::new(SnapshotStore::new());
        let activity = Arc::new(ActivitySignal::new());
        let stop = Arc::new(AtomicBool::new(false));
        let metrics = Arc::new(PipelineMetrics::new());

        let worker_ring = Arc::clone(&ring);
        let worker_snapshots = Arc::clone(&snapshots);
        let worker_activity = Arc::clone(&activity);
        let worker_stop = Arc::clone(&stop);
        let worker_metrics = Arc::clone(&metrics);
        let worker = thread::Builder::new()
            .name("fft-plug-dsp".to_string())
            .spawn(move || {
                run_dsp_worker(
                    worker_ring,
                    worker_snapshots,
                    worker_activity,
                    worker_stop,
                    config,
                    worker_metrics,
                )
            })
            .map_err(PipelineError::Worker)?;
        ring.register_worker(worker.thread().clone());

        Ok(Arc::new(Self {
            ring,
            snapshots,
            activity,
            stop,
            metrics,
            worker: Mutex::new(Some(worker)),
        }))
    }

    pub fn ring(&self) -> Arc<PcmRing> {
        Arc::clone(&self.ring)
    }

    pub fn latest_snapshot(&self) -> Option<Arc<SpectrumSnapshot>> {
        self.snapshots.latest()
    }

    pub fn activity_state(&self) -> ActivityState {
        self.activity.state()
    }

    pub fn take_wake_request(&self) -> bool {
        self.activity.take_wake_request()
    }

    pub fn ring_stats(&self) -> RingStats {
        self.ring.stats()
    }

    pub fn stats(&self) -> PipelineStats {
        PipelineStats {
            ring: self.ring.stats(),
            dsp_frames: self.metrics.dsp_frames.load(Ordering::Relaxed),
            last_peak_hz: f32::from_bits(self.metrics.last_peak_hz_bits.load(Ordering::Relaxed)),
            last_peak_level: f32::from_bits(
                self.metrics.last_peak_level_bits.load(Ordering::Relaxed),
            ),
            activity: self.activity.state(),
        }
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::Release);
        self.ring.wake_worker();
        if let Ok(mut worker) = self.worker.lock()
            && let Some(worker) = worker.take()
        {
            let _ = worker.join();
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.ring.wake_worker();
        if let Ok(worker) = self.worker.get_mut()
            && let Some(worker) = worker.take()
        {
            let _ = worker.join();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SilenceTracker {
    state: ActivityState,
    quiet_seconds: f32,
}

impl Default for SilenceTracker {
    fn default() -> Self {
        Self {
            state: ActivityState::Active,
            quiet_seconds: 0.0,
        }
    }
}

impl SilenceTracker {
    fn sleeping() -> Self {
        Self {
            state: ActivityState::Sleeping,
            quiet_seconds: SLEEP_AFTER_SECONDS,
        }
    }

    pub fn state(&self) -> ActivityState {
        self.state
    }

    pub fn advance(&mut self, peak_linear: f32, elapsed_seconds: f32) -> ActivityState {
        let peak_linear = if peak_linear.is_finite() {
            peak_linear.max(0.0)
        } else {
            0.0
        };
        let elapsed_seconds = if elapsed_seconds.is_finite() {
            elapsed_seconds.max(0.0)
        } else {
            0.0
        };

        let active_threshold = if self.state == ActivityState::Sleeping {
            WAKE_THRESHOLD
        } else {
            QUIET_THRESHOLD
        };
        if peak_linear >= active_threshold {
            self.state = ActivityState::Active;
            self.quiet_seconds = 0.0;
            return self.state;
        }

        self.quiet_seconds += elapsed_seconds;
        if self.quiet_seconds >= SLEEP_AFTER_SECONDS {
            self.state = ActivityState::Sleeping;
        } else if self.quiet_seconds >= DECAY_START_SECONDS {
            self.state = ActivityState::Decaying;
        }
        self.state
    }
}

fn run_dsp_worker(
    ring: Arc<PcmRing>,
    snapshots: Arc<SnapshotStore>,
    activity: Arc<ActivitySignal>,
    stop: Arc<AtomicBool>,
    config: DspConfig,
    metrics: Arc<PipelineMetrics>,
) {
    let mut analyzer: Option<Analyzer> = None;
    let mut silence = SilenceTracker::sleeping();

    while !stop.load(Ordering::Acquire) {
        loop {
            if stop.load(Ordering::Acquire) {
                break;
            }
            if ring.take_discontinuity()
                && let Some(analyzer) = analyzer.as_mut()
            {
                analyzer.reset();
            }
            let processed = ring.with_next(|block| {
                let sample_rate = block.sample_rate();
                let samples = block.samples();
                if sample_rate == 0 || samples.is_empty() {
                    return;
                }

                if analyzer
                    .as_ref()
                    .is_none_or(|current: &Analyzer| current.sample_rate() != sample_rate)
                {
                    analyzer = Analyzer::new(sample_rate, config.clone()).ok();
                }

                let peak = samples
                    .iter()
                    .map(|sample| {
                        if sample.is_finite() {
                            sample.abs()
                        } else {
                            0.0
                        }
                    })
                    .fold(0.0_f32, f32::max);
                let previous_state = silence.state();
                let elapsed = samples.len() as f32 / sample_rate as f32;
                let state = silence.advance(peak, elapsed);
                activity.set_state(state);

                if state == ActivityState::Sleeping {
                    return;
                }
                if previous_state == ActivityState::Sleeping && state == ActivityState::Active {
                    activity.request_wake();
                }

                let Some(analyzer) = analyzer.as_mut() else {
                    return;
                };
                if let Ok(Some(snapshot)) = analyzer.process(samples) {
                    if let Some(peak) = snapshot.bands.iter().max_by(|left, right| {
                        left.normalized_level
                            .partial_cmp(&right.normalized_level)
                            .unwrap_or(std::cmp::Ordering::Equal)
                    }) {
                        metrics
                            .last_peak_hz_bits
                            .store(peak.center_hz.to_bits(), Ordering::Relaxed);
                        metrics
                            .last_peak_level_bits
                            .store(peak.normalized_level.to_bits(), Ordering::Relaxed);
                    }
                    metrics.dsp_frames.fetch_add(1, Ordering::Relaxed);
                    snapshots.publish(snapshot);
                }
            });

            if processed.is_none() {
                break;
            }
        }

        if stop.load(Ordering::Acquire) {
            break;
        }
        if ring.prepare_to_park() {
            continue;
        }
        thread::park();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn fifo_preserves_block_order_and_does_not_repeat() {
        let ring = PcmRing::new(4).expect("valid ring");
        for value in [1.0, 2.0, 3.0] {
            assert!(ring.push_with(1, 48_000, |_| value));
        }

        assert_eq!(ring.with_next(|block| block.samples()[0]), Some(1.0));
        assert_eq!(ring.with_next(|block| block.samples()[0]), Some(2.0));
        assert_eq!(ring.with_next(|block| block.samples()[0]), Some(3.0));
        assert!(ring.with_next(|_| ()).is_none());
    }

    #[test]
    fn fifo_drops_oldest_ready_block_and_marks_discontinuity() {
        let ring = PcmRing::new(1).expect("valid ring");
        for value in 1..=PCM_RING_SLOTS + 1 {
            assert!(ring.push_with(1, 48_000, |_| value as f32));
        }

        assert!(ring.take_discontinuity());
        for expected in 2..=PCM_RING_SLOTS + 1 {
            assert_eq!(
                ring.with_next(|block| block.samples()[0]),
                Some(expected as f32)
            );
        }
        assert!(ring.with_next(|_| ()).is_none());
        assert!(ring.stats().overwritten_blocks >= 1);
    }

    #[test]
    fn reader_holds_a_slot_until_processing_finishes() {
        let ring = PcmRing::new(2).expect("valid ring");
        assert!(ring.push_with(1, 48_000, |_| 1.0));
        let held = ring.with_next(|block| {
            assert_eq!(block.frames(), 1);
            assert_eq!(block.samples()[0], 1.0);
            for value in 2..=PCM_RING_SLOTS {
                assert!(ring.push_with(1, 48_000, |_| value as f32));
            }
            assert!(!ring.push_with(1, 48_000, |_| { (PCM_RING_SLOTS + 1) as f32 }));
            assert_eq!(block.samples()[0], 1.0);
        });
        assert!(held.is_some());
        assert_eq!(ring.with_next(|block| block.samples()[0]), Some(2.0));
        assert_eq!(ring.with_next(|block| block.samples()[0]), Some(3.0));
        for expected in 4..=PCM_RING_SLOTS {
            assert_eq!(
                ring.with_next(|block| block.samples()[0]),
                Some(expected as f32)
            );
        }
        assert!(ring.with_next(|_| ()).is_none());
    }

    #[test]
    fn silence_tracker_decays_sleeps_and_wakes() {
        let mut tracker = SilenceTracker::default();
        assert_eq!(tracker.advance(0.0, 0.2), ActivityState::Decaying);
        assert_eq!(tracker.advance(0.0, 0.4), ActivityState::Sleeping);
        assert_eq!(tracker.advance(0.001, 0.01), ActivityState::Sleeping);
        assert_eq!(tracker.advance(0.01, 0.01), ActivityState::Active);
    }

    #[test]
    fn pcm_push_unparks_registered_worker() {
        let ring = Arc::new(PcmRing::new(1).expect("valid ring"));
        let (ready_sender, ready_receiver) = mpsc::channel();
        let worker_ring = Arc::clone(&ring);
        let worker = thread::spawn(move || {
            worker_ring.register_worker(thread::current());
            ready_sender.send(()).expect("main thread is listening");
            thread::park();
        });

        ready_receiver
            .recv_timeout(Duration::from_secs(1))
            .expect("worker should register");
        assert!(ring.push_with(1, 48_000, |_| 1.0));
        worker.join().expect("worker should be unparked");
    }

    #[test]
    fn pipeline_shutdown_unparks_idle_worker() {
        let pipeline = Pipeline::start(DspConfig::default()).expect("worker should start");
        pipeline.shutdown();
    }
}

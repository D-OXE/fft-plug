use std::convert::TryInto;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use pipewire as pw;
use pw::properties::properties;
use pw::spa::buffer::Data;
use pw::spa::param::audio::{AudioFormat, AudioInfoRaw};
use pw::spa::param::format::{MediaSubtype, MediaType};
use pw::spa::param::format_utils;
use pw::spa::pod::Pod;
use pw::spa::pod::serialize::PodSerializer;
use pw::spa::utils::Direction;

use fft_plug::pipeline::PcmRing;

const STATE_STARTING: u8 = 0;
const STATE_CONNECTED: u8 = 1;
const STATE_PAUSED: u8 = 2;
const STATE_ERROR: u8 = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureState {
    Starting,
    Connected,
    Paused,
    Error,
}

impl CaptureState {
    fn from_raw(value: u8) -> Self {
        match value {
            STATE_CONNECTED => Self::Connected,
            STATE_PAUSED => Self::Paused,
            STATE_ERROR => Self::Error,
            _ => Self::Starting,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AudioStats {
    pub state: CaptureState,
    pub sample_rate: u32,
    pub channels: u32,
    pub format_code: u32,
    pub callbacks: u64,
    pub callback_drops: u64,
    pub unsupported_buffers: u64,
    pub peak: f32,
    /// Number of PipeWire sessions this process has started; the first one is generation 1.
    pub session_generation: u64,
    /// Session restarts after the first one, i.e. how often capture had to be re-established.
    pub reconnects: u64,
}

pub struct AudioStatus {
    state: AtomicU8,
    sample_rate: AtomicU32,
    channels: AtomicU32,
    format_code: AtomicU32,
    format_generation: AtomicU64,
    callbacks: AtomicU64,
    callback_drops: AtomicU64,
    unsupported_buffers: AtomicU64,
    peak_bits: AtomicU32,
    session_generation: AtomicU64,
    reconnects: AtomicU64,
}

impl AudioStatus {
    pub fn new() -> Self {
        Self {
            state: AtomicU8::new(STATE_STARTING),
            sample_rate: AtomicU32::new(0),
            channels: AtomicU32::new(0),
            format_code: AtomicU32::new(AudioFormat::Unknown.as_raw()),
            format_generation: AtomicU64::new(0),
            callbacks: AtomicU64::new(0),
            callback_drops: AtomicU64::new(0),
            unsupported_buffers: AtomicU64::new(0),
            peak_bits: AtomicU32::new(0.0_f32.to_bits()),
            session_generation: AtomicU64::new(0),
            reconnects: AtomicU64::new(0),
        }
    }

    /// Counts a capture session start. Called only by the PipeWire management thread before each
    /// `run_session`, never from the realtime process callback.
    fn begin_session(&self) {
        let generation = self
            .session_generation
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        if generation > 1 {
            self.reconnects.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn set_state(&self, state: CaptureState) {
        let value = match state {
            CaptureState::Starting => STATE_STARTING,
            CaptureState::Connected => STATE_CONNECTED,
            CaptureState::Paused => STATE_PAUSED,
            CaptureState::Error => STATE_ERROR,
        };
        self.state.store(value, Ordering::Release);
    }

    fn set_format(&self, info: AudioInfoRaw) {
        self.sample_rate.store(info.rate(), Ordering::Release);
        self.channels.store(info.channels(), Ordering::Release);
        self.format_code
            .store(info.format().as_raw(), Ordering::Release);
        self.format_generation.fetch_add(1, Ordering::AcqRel);
    }

    fn clear_format(&self) {
        self.sample_rate.store(0, Ordering::Release);
        self.channels.store(0, Ordering::Release);
        self.format_code
            .store(AudioFormat::Unknown.as_raw(), Ordering::Release);
        self.format_generation.fetch_add(1, Ordering::AcqRel);
    }

    fn record_callback(&self) {
        self.callbacks.fetch_add(1, Ordering::Relaxed);
    }

    fn record_callback_drop(&self) {
        self.callback_drops.fetch_add(1, Ordering::Relaxed);
    }

    fn record_unsupported_buffer(&self) {
        self.unsupported_buffers.fetch_add(1, Ordering::Relaxed);
    }

    fn record_peak(&self, peak: f32) {
        if peak.is_finite() {
            self.peak_bits
                .fetch_max(peak.max(0.0).to_bits(), Ordering::Relaxed);
        }
    }

    pub fn snapshot(&self) -> AudioStats {
        AudioStats {
            state: CaptureState::from_raw(self.state.load(Ordering::Acquire)),
            sample_rate: self.sample_rate.load(Ordering::Acquire),
            channels: self.channels.load(Ordering::Acquire),
            format_code: self.format_code.load(Ordering::Acquire),
            callbacks: self.callbacks.load(Ordering::Relaxed),
            callback_drops: self.callback_drops.load(Ordering::Relaxed),
            unsupported_buffers: self.unsupported_buffers.load(Ordering::Relaxed),
            peak: f32::from_bits(self.peak_bits.swap(0, Ordering::AcqRel)),
            session_generation: self.session_generation.load(Ordering::Acquire),
            reconnects: self.reconnects.load(Ordering::Relaxed),
        }
    }
}

impl Default for AudioStatus {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub enum AudioError {
    PipeWire(String),
    Startup(String),
    Thread(String),
}

impl fmt::Display for AudioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PipeWire(error) => write!(f, "PipeWire error: {error}"),
            Self::Startup(error) => write!(f, "audio capture could not start: {error}"),
            Self::Thread(error) => write!(f, "audio capture thread failed: {error}"),
        }
    }
}

impl std::error::Error for AudioError {}

impl From<pw::Error> for AudioError {
    fn from(error: pw::Error) -> Self {
        Self::PipeWire(error.to_string())
    }
}

pub struct AudioCapture {
    stop: Arc<AtomicBool>,
    stop_sender: Option<pw::channel::Sender<()>>,
    thread: Option<JoinHandle<Result<(), AudioError>>>,
}

impl AudioCapture {
    pub fn start(ring: Arc<PcmRing>, status: Arc<AudioStatus>) -> Result<Self, AudioError> {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let thread_stop = Arc::clone(&stop);
        let thread_ring = Arc::clone(&ring);
        let thread_status = Arc::clone(&status);
        let thread = thread::Builder::new()
            .name("fft-plug-pipewire".to_string())
            .spawn(move || {
                run_pipewire_thread(thread_ring, thread_status, thread_stop, ready_sender)
            })
            .map_err(|error| AudioError::Thread(error.to_string()))?;

        let stop_sender = match ready_receiver.recv() {
            Ok(Ok(sender)) => sender,
            Ok(Err(error)) => {
                let _ = thread.join();
                return Err(error);
            }
            Err(error) => {
                let _ = thread.join();
                return Err(AudioError::Startup(error.to_string()));
            }
        };

        Ok(Self {
            stop,
            stop_sender: Some(stop_sender),
            thread: Some(thread),
        })
    }

    pub fn stop(&mut self) -> Result<(), AudioError> {
        self.stop.store(true, Ordering::Release);
        if let Some(sender) = self.stop_sender.take() {
            let _ = sender.send(());
        }

        let Some(thread) = self.thread.take() else {
            return Ok(());
        };
        match thread.join() {
            Ok(result) => result,
            Err(_) => Err(AudioError::Thread(
                "PipeWire thread panicked during shutdown".to_string(),
            )),
        }
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

struct CaptureData {
    format: AudioInfoRaw,
    ring: Arc<PcmRing>,
    status: Arc<AudioStatus>,
    restart: Arc<AtomicBool>,
}

fn run_pipewire_thread(
    ring: Arc<PcmRing>,
    status: Arc<AudioStatus>,
    stop: Arc<AtomicBool>,
    ready_sender: SyncSender<Result<pw::channel::Sender<()>, AudioError>>,
) -> Result<(), AudioError> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let (stop_sender, stop_receiver) = pw::channel::channel();
    let stop_loop = mainloop.clone();
    let _stop_receiver = stop_receiver.attach(mainloop.loop_(), move |_| stop_loop.quit());
    let mut ready_sender = Some(ready_sender);

    while !stop.load(Ordering::Acquire) {
        status.begin_session();
        status.set_state(CaptureState::Starting);
        let restart = Arc::new(AtomicBool::new(false));
        let session = run_session(
            &mainloop,
            Arc::clone(&ring),
            Arc::clone(&status),
            Arc::clone(&restart),
            &stop_sender,
            &mut ready_sender,
        );

        if let Err(error) = session {
            status.set_state(CaptureState::Error);
            if let Some(sender) = ready_sender.take() {
                let _ = sender.send(Err(AudioError::Startup(error.to_string())));
                return Err(error);
            }
        }

        if stop.load(Ordering::Acquire) {
            break;
        }

        if !restart.load(Ordering::Acquire) {
            status.set_state(CaptureState::Error);
        }
        for _ in 0..10 {
            if stop.load(Ordering::Acquire) {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    Ok(())
}

fn run_session(
    mainloop: &pw::main_loop::MainLoopRc,
    ring: Arc<PcmRing>,
    status: Arc<AudioStatus>,
    restart: Arc<AtomicBool>,
    stop_sender: &pw::channel::Sender<()>,
    ready_sender: &mut Option<SyncSender<Result<pw::channel::Sender<()>, AudioError>>>,
) -> Result<(), AudioError> {
    let context = pw::context::ContextRc::new(mainloop, None)?;
    let core = context.connect_rc(None)?;
    let props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Music",
        *pw::keys::STREAM_CAPTURE_SINK => "true",
    };
    let stream = pw::stream::StreamBox::new(&core, "fft-plug-monitor", props)?;
    let data = CaptureData {
        format: AudioInfoRaw::new(),
        ring,
        status: Arc::clone(&status),
        restart: Arc::clone(&restart),
    };

    let _listener = stream
        .add_local_listener_with_user_data(data)
        .state_changed({
            let mainloop = mainloop.clone();
            move |_, data, _, new| match new {
                pw::stream::StreamState::Streaming => {
                    data.status.set_state(CaptureState::Connected)
                }
                pw::stream::StreamState::Paused => data.status.set_state(CaptureState::Paused),
                pw::stream::StreamState::Error(_) | pw::stream::StreamState::Unconnected => {
                    data.status.set_state(CaptureState::Error);
                    data.restart.store(true, Ordering::Release);
                    mainloop.quit();
                }
                pw::stream::StreamState::Connecting => {
                    data.status.set_state(CaptureState::Starting)
                }
            }
        })
        .param_changed(|_, data, id, param| {
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Some(param) = param else {
                data.status.clear_format();
                return;
            };
            let Ok((media_type, media_subtype)) = format_utils::parse_format(param) else {
                return;
            };
            if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
                return;
            }
            if data.format.parse(param).is_ok() {
                data.status.set_format(data.format);
            }
        })
        .process(process_buffer)
        .register()
        .map_err(AudioError::from)?;

    let mut audio_info = AudioInfoRaw::new();
    audio_info.set_format(AudioFormat::F32LE);
    let object = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let serialized = PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(object),
    )
    .map_err(|error| AudioError::Startup(error.to_string()))?
    .0
    .into_inner();
    let pod = Pod::from_bytes(&serialized)
        .ok_or_else(|| AudioError::Startup("could not build PipeWire format pod".to_string()))?;
    let mut params = [pod];

    stream.connect(
        Direction::Input,
        None,
        pw::stream::StreamFlags::AUTOCONNECT
            | pw::stream::StreamFlags::MAP_BUFFERS
            | pw::stream::StreamFlags::RT_PROCESS,
        &mut params,
    )?;

    if let Some(sender) = ready_sender.take() {
        let _ = sender.send(Ok(stop_sender.clone()));
    }

    mainloop.run();
    Ok(())
}

fn process_buffer(stream: &pw::stream::Stream, data: &mut CaptureData) {
    data.status.record_callback();
    let Some(mut buffer) = stream.dequeue_buffer() else {
        data.status.record_callback_drop();
        return;
    };
    let datas = buffer.datas_mut();
    let channels = usize::try_from(data.format.channels()).unwrap_or(0);
    if channels == 0 {
        data.status.record_callback_drop();
        return;
    }
    let format = data.format.format();
    let Some(bytes_per_sample) = bytes_per_sample(format) else {
        data.status.record_unsupported_buffer();
        return;
    };
    let sample_rate = data.format.rate();

    if format.is_planar() {
        process_planar_buffer(
            datas,
            channels,
            format,
            bytes_per_sample,
            sample_rate,
            &data.ring,
            &data.status,
        );
        return;
    }
    if !format.is_interleaved() {
        data.status.record_unsupported_buffer();
        return;
    }

    let Some(data_area) = datas.first_mut() else {
        data.status.record_callback_drop();
        return;
    };
    process_interleaved_buffer(
        data_area,
        channels,
        format,
        bytes_per_sample,
        sample_rate,
        &data.ring,
        &data.status,
    );
}

#[derive(Clone, Copy)]
struct DataLayout {
    offset: usize,
    end: usize,
    stride: usize,
}

fn data_layout(data_area: &mut Data, bytes_per_sample: usize) -> Option<DataLayout> {
    if data_area.as_raw().chunk.is_null() {
        return None;
    }
    let (offset, size, raw_stride) = {
        let chunk = data_area.chunk();
        (
            chunk.offset() as usize,
            chunk.size() as usize,
            chunk.stride(),
        )
    };
    let bytes = data_area.data()?;
    let end = offset.checked_add(size)?.min(bytes.len());
    if offset >= end {
        return None;
    }
    let stride = usize::try_from(raw_stride)
        .ok()
        .filter(|stride| *stride >= bytes_per_sample)
        .unwrap_or(bytes_per_sample);
    Some(DataLayout {
        offset,
        end,
        stride,
    })
}

fn process_interleaved_buffer(
    data_area: &mut Data,
    channels: usize,
    format: AudioFormat,
    bytes_per_sample: usize,
    sample_rate: u32,
    ring: &PcmRing,
    status: &AudioStatus,
) {
    let Some(layout) = data_layout(data_area, bytes_per_sample) else {
        status.record_callback_drop();
        return;
    };
    let Some(bytes) = data_area.data() else {
        status.record_callback_drop();
        return;
    };
    let bytes = &bytes[layout.offset..layout.end];
    let default_stride = channels.saturating_mul(bytes_per_sample);
    let stride = layout.stride.max(default_stride).max(bytes_per_sample);
    let frame_count = bytes.len() / stride;
    if frame_count == 0 {
        return;
    }
    let mut peak = 0.0_f32;
    let published = ring.push_with(frame_count, sample_rate, |frame| {
        let frame_start = frame * stride;
        let mut sum = 0.0_f32;
        for channel in 0..channels {
            let start = frame_start + channel * bytes_per_sample;
            let end = start + bytes_per_sample;
            if end <= bytes.len() {
                sum += decode_sample(&bytes[start..end], format).unwrap_or(0.0);
            }
        }
        let sample = (sum / channels as f32).clamp(-1.0, 1.0);
        peak = peak.max(sample.abs());
        sample
    });
    status.record_peak(peak);
    if !published {
        status.record_callback_drop();
    }
}

fn process_planar_buffer(
    datas: &mut [Data],
    channels: usize,
    format: AudioFormat,
    bytes_per_sample: usize,
    sample_rate: u32,
    ring: &PcmRing,
    status: &AudioStatus,
) {
    if datas.len() < channels {
        status.record_callback_drop();
        return;
    }

    let mut frame_count = usize::MAX;
    for data_area in datas.iter_mut().take(channels) {
        let Some(layout) = data_layout(data_area, bytes_per_sample) else {
            status.record_callback_drop();
            return;
        };
        frame_count = frame_count.min((layout.end - layout.offset) / layout.stride);
    }
    if frame_count == 0 || frame_count == usize::MAX {
        return;
    }

    let mut peak = 0.0_f32;
    let published = ring.push_with(frame_count, sample_rate, |frame| {
        let mut sum = 0.0_f32;
        for data_area in datas.iter_mut().take(channels) {
            let Some(layout) = data_layout(data_area, bytes_per_sample) else {
                continue;
            };
            let Some(bytes) = data_area.data() else {
                continue;
            };
            let Some(start) = layout
                .offset
                .checked_add(frame.saturating_mul(layout.stride))
            else {
                continue;
            };
            let Some(end) = start.checked_add(bytes_per_sample) else {
                continue;
            };
            if end <= layout.end && end <= bytes.len() {
                sum += decode_sample(&bytes[start..end], format).unwrap_or(0.0);
            }
        }
        let sample = (sum / channels as f32).clamp(-1.0, 1.0);
        peak = peak.max(sample.abs());
        sample
    });
    status.record_peak(peak);
    if !published {
        status.record_callback_drop();
    }
}

fn bytes_per_sample(format: AudioFormat) -> Option<usize> {
    match format {
        AudioFormat::S8 | AudioFormat::U8 | AudioFormat::S8P | AudioFormat::U8P => Some(1),
        AudioFormat::S16LE
        | AudioFormat::S16BE
        | AudioFormat::U16LE
        | AudioFormat::U16BE
        | AudioFormat::S16P => Some(2),
        AudioFormat::S24_32LE
        | AudioFormat::S24_32BE
        | AudioFormat::U24_32LE
        | AudioFormat::U24_32BE
        | AudioFormat::S32LE
        | AudioFormat::S32BE
        | AudioFormat::U32LE
        | AudioFormat::U32BE
        | AudioFormat::S24_32P
        | AudioFormat::S32P => Some(4),
        AudioFormat::F32LE | AudioFormat::F32BE | AudioFormat::F32P => Some(4),
        AudioFormat::F64LE | AudioFormat::F64BE | AudioFormat::F64P => Some(8),
        _ => None,
    }
}

fn decode_sample(bytes: &[u8], format: AudioFormat) -> Option<f32> {
    let value = match format {
        AudioFormat::F32LE => f32::from_le_bytes(bytes.try_into().ok()?),
        AudioFormat::F32BE => f32::from_be_bytes(bytes.try_into().ok()?),
        AudioFormat::F64LE => f64::from_le_bytes(bytes.try_into().ok()?) as f32,
        AudioFormat::F64BE => f64::from_be_bytes(bytes.try_into().ok()?) as f32,
        AudioFormat::S8 => i8::from_ne_bytes(bytes.try_into().ok()?) as f32 / 128.0,
        AudioFormat::U8 => bytes.first().copied()? as f32 / 128.0 - 1.0,
        AudioFormat::S8P => i8::from_ne_bytes(bytes.try_into().ok()?) as f32 / 128.0,
        AudioFormat::U8P => bytes.first().copied()? as f32 / 128.0 - 1.0,
        AudioFormat::S16LE => i16::from_le_bytes(bytes.try_into().ok()?) as f32 / 32_768.0,
        AudioFormat::S16BE => i16::from_be_bytes(bytes.try_into().ok()?) as f32 / 32_768.0,
        AudioFormat::U16LE => u16::from_le_bytes(bytes.try_into().ok()?) as f32 / 32_768.0 - 1.0,
        AudioFormat::U16BE => u16::from_be_bytes(bytes.try_into().ok()?) as f32 / 32_768.0 - 1.0,
        AudioFormat::S16P => i16::from_ne_bytes(bytes.try_into().ok()?) as f32 / 32_768.0,
        AudioFormat::S32LE => i32::from_le_bytes(bytes.try_into().ok()?) as f32 / 2_147_483_648.0,
        AudioFormat::S32BE => i32::from_be_bytes(bytes.try_into().ok()?) as f32 / 2_147_483_648.0,
        AudioFormat::U32LE => {
            u32::from_le_bytes(bytes.try_into().ok()?) as f32 / 2_147_483_648.0 - 1.0
        }
        AudioFormat::U32BE => {
            u32::from_be_bytes(bytes.try_into().ok()?) as f32 / 2_147_483_648.0 - 1.0
        }
        AudioFormat::S24_32P | AudioFormat::S32P => {
            i32::from_ne_bytes(bytes.try_into().ok()?) as f32 / 2_147_483_648.0
        }
        AudioFormat::F32P => f32::from_ne_bytes(bytes.try_into().ok()?),
        AudioFormat::F64P => f64::from_ne_bytes(bytes.try_into().ok()?) as f32,
        _ => return None,
    };
    Some(if value.is_finite() {
        value.clamp(-1.0, 1.0)
    } else {
        0.0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_status_counts_session_generations_and_reconnects() {
        let status = AudioStatus::new();
        let initial = status.snapshot();
        assert_eq!(initial.session_generation, 0);
        assert_eq!(initial.reconnects, 0);

        status.begin_session();
        let first = status.snapshot();
        assert_eq!(first.session_generation, 1);
        assert_eq!(first.reconnects, 0);

        status.begin_session();
        let second = status.snapshot();
        assert_eq!(second.session_generation, 2);
        assert_eq!(second.reconnects, 1);
    }
}

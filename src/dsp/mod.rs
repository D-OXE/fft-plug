use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use realfft::{RealFftPlanner, RealToComplex};
use rustfft::num_complex::Complex;

pub const DEFAULT_FFT_SIZE: usize = 4096;
pub const DEFAULT_HOP_SIZE: usize = 512;
pub const DEFAULT_BAND_COUNT: usize = 64;
pub const DEFAULT_MIN_FREQUENCY: f32 = 20.0;
pub const DEFAULT_MAX_FREQUENCY: f32 = 20_000.0;
pub const DEFAULT_MIN_DB: f32 = -80.0;
pub const DEFAULT_ATTACK_SECONDS: f32 = 0.015;
pub const DEFAULT_RELEASE_SECONDS: f32 = 0.12;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DynamicsProfile {
    Soft,
    Balanced,
    #[default]
    Punchy,
}

impl DynamicsProfile {
    pub const fn attack_seconds(self) -> f32 {
        match self {
            Self::Soft => 0.045,
            Self::Balanced => 0.025,
            Self::Punchy => 0.015,
        }
    }

    pub const fn release_seconds(self) -> f32 {
        match self {
            Self::Soft => 0.22,
            Self::Balanced => 0.15,
            Self::Punchy => 0.12,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DspConfig {
    pub fft_size: usize,
    pub hop_size: usize,
    pub band_count: usize,
    pub min_frequency: f32,
    pub max_frequency: f32,
    pub min_db: f32,
    pub attack_seconds: f32,
    pub release_seconds: f32,
}

impl Default for DspConfig {
    fn default() -> Self {
        Self {
            fft_size: DEFAULT_FFT_SIZE,
            hop_size: DEFAULT_HOP_SIZE,
            band_count: DEFAULT_BAND_COUNT,
            min_frequency: DEFAULT_MIN_FREQUENCY,
            max_frequency: DEFAULT_MAX_FREQUENCY,
            min_db: DEFAULT_MIN_DB,
            attack_seconds: DEFAULT_ATTACK_SECONDS,
            release_seconds: DEFAULT_RELEASE_SECONDS,
        }
    }
}

impl DspConfig {
    pub fn with_dynamics(mut self, profile: DynamicsProfile) -> Self {
        self.attack_seconds = profile.attack_seconds();
        self.release_seconds = profile.release_seconds();
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DspError {
    InvalidConfig(String),
    InvalidSampleRate,
    InvalidChannelCount,
    Fft(String),
}

impl fmt::Display for DspError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(message) => write!(f, "invalid DSP configuration: {message}"),
            Self::InvalidSampleRate => f.write_str("sample rate must be greater than zero"),
            Self::InvalidChannelCount => f.write_str("channel count must be greater than zero"),
            Self::Fft(message) => write!(f, "FFT processing failed: {message}"),
        }
    }
}

impl Error for DspError {}

#[derive(Clone, Debug, PartialEq)]
pub struct FrequencyBand {
    pub center_hz: f32,
    pub level_db: f32,
    pub normalized_level: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SpectrumSnapshot {
    pub sample_rate: u32,
    pub frame_index: u64,
    pub bands: Vec<FrequencyBand>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct BandRange {
    pub start_bin: usize,
    pub end_bin: usize,
    pub center_hz: f32,
}

impl BandRange {
    pub fn bin_count(&self) -> usize {
        self.end_bin.saturating_sub(self.start_bin) + 1
    }
}

#[derive(Clone, Debug)]
pub struct AttackRelease {
    value: f32,
    attack_seconds: f32,
    release_seconds: f32,
}

impl AttackRelease {
    pub fn new(attack_seconds: f32, release_seconds: f32) -> Result<Self, DspError> {
        if !attack_seconds.is_finite()
            || !release_seconds.is_finite()
            || attack_seconds <= 0.0
            || release_seconds <= 0.0
        {
            return Err(DspError::InvalidConfig(
                "attack and release times must be finite and positive".to_string(),
            ));
        }

        Ok(Self {
            value: 0.0,
            attack_seconds,
            release_seconds,
        })
    }

    pub fn value(&self) -> f32 {
        self.value
    }

    pub fn advance(&mut self, target: f32, elapsed_seconds: f32) -> f32 {
        let target = if target.is_finite() {
            target.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let elapsed_seconds = if elapsed_seconds.is_finite() {
            elapsed_seconds.max(0.0)
        } else {
            0.0
        };
        let time_constant = if target >= self.value {
            self.attack_seconds
        } else {
            self.release_seconds
        };
        let alpha = 1.0 - (-elapsed_seconds / time_constant).exp();
        self.value += (target - self.value) * alpha.clamp(0.0, 1.0);
        self.value = self.value.clamp(0.0, 1.0);
        self.value
    }
}

pub fn mix_interleaved_to_mono(
    input: &[f32],
    channels: usize,
    output: &mut Vec<f32>,
) -> Result<usize, DspError> {
    if channels == 0 {
        return Err(DspError::InvalidChannelCount);
    }

    let frame_count = input.len() / channels;
    output.clear();
    output.resize(frame_count, 0.0);

    for (frame_index, output_sample) in output.iter_mut().enumerate() {
        let start = frame_index * channels;
        let sum = input[start..start + channels]
            .iter()
            .map(|sample| if sample.is_finite() { *sample } else { 0.0 })
            .sum::<f32>();
        *output_sample = (sum / channels as f32).clamp(-1.0, 1.0);
    }

    Ok(frame_count)
}

pub struct Analyzer {
    config: DspConfig,
    sample_rate: u32,
    fft: Arc<dyn RealToComplex<f32>>,
    fft_input: Vec<f32>,
    fft_output: Vec<Complex<f32>>,
    window: Vec<f32>,
    window_gain: f32,
    samples: VecDeque<f32>,
    band_ranges: Vec<BandRange>,
    raw_band_levels: Vec<f32>,
    smoothers: Vec<AttackRelease>,
    frame_index: u64,
}

impl Analyzer {
    pub fn new(sample_rate: u32, config: DspConfig) -> Result<Self, DspError> {
        validate_config(sample_rate, &config)?;

        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(config.fft_size);
        let fft_input = fft.make_input_vec();
        let fft_output = fft.make_output_vec();

        let window = hann_window(config.fft_size);
        let window_gain = window.iter().sum::<f32>();
        if window_gain <= f32::EPSILON || !window_gain.is_finite() {
            return Err(DspError::InvalidConfig(
                "FFT window has no usable coherent gain".to_string(),
            ));
        }

        let band_ranges = build_band_ranges(
            sample_rate,
            config.fft_size,
            config.min_frequency,
            config.max_frequency,
            config.band_count,
        );
        let smoothers = (0..band_ranges.len())
            .map(|_| AttackRelease::new(config.attack_seconds, config.release_seconds))
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Self {
            raw_band_levels: vec![config.min_db; band_ranges.len()],
            samples: VecDeque::with_capacity(config.fft_size),
            config,
            sample_rate,
            fft,
            fft_input,
            fft_output,
            window,
            window_gain,
            band_ranges,
            smoothers,
            frame_index: 0,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn config(&self) -> &DspConfig {
        &self.config
    }

    pub fn band_ranges(&self) -> &[BandRange] {
        &self.band_ranges
    }

    /// Discard samples and smoothing state after a discontinuity in the PCM stream.
    pub fn reset(&mut self) {
        self.samples.clear();
        self.raw_band_levels.fill(self.config.min_db);
        for smoother in &mut self.smoothers {
            smoother.value = 0.0;
        }
    }

    pub fn process(&mut self, mono_samples: &[f32]) -> Result<Option<SpectrumSnapshot>, DspError> {
        let mut latest = None;

        for sample in mono_samples {
            self.samples
                .push_back(if sample.is_finite() { *sample } else { 0.0 });

            if self.samples.len() == self.config.fft_size {
                latest = Some(self.analyze_window()?);
                for _ in 0..self.config.hop_size {
                    let _ = self.samples.pop_front();
                }
            }
        }

        Ok(latest)
    }

    fn analyze_window(&mut self) -> Result<SpectrumSnapshot, DspError> {
        for (index, sample) in self.samples.iter().enumerate() {
            self.fft_input[index] = *sample * self.window[index];
        }

        self.fft
            .process(&mut self.fft_input, &mut self.fft_output)
            .map_err(|error| DspError::Fft(error.to_string()))?;

        self.raw_band_levels.fill(self.config.min_db);
        for (band_index, range) in self.band_ranges.iter().enumerate() {
            let mut peak_db = self.config.min_db;
            for bin in range.start_bin..=range.end_bin {
                let Some(value) = self.fft_output.get(bin) else {
                    continue;
                };
                peak_db = peak_db.max(magnitude_to_db(
                    value.norm(),
                    bin,
                    self.config.fft_size,
                    self.window_gain,
                    self.config.min_db,
                ));
            }
            self.raw_band_levels[band_index] = finite_or(peak_db, self.config.min_db);
        }

        let elapsed_seconds = self.config.hop_size as f32 / self.sample_rate as f32;
        let bands = self
            .raw_band_levels
            .iter()
            .zip(self.smoothers.iter_mut())
            .zip(self.band_ranges.iter())
            .map(|((level_db, smoother), range)| {
                let normalized = normalize_db(*level_db, self.config.min_db);
                let smoothed = smoother.advance(normalized, elapsed_seconds);
                let smoothed_db = self.config.min_db + smoothed * -self.config.min_db;
                FrequencyBand {
                    center_hz: range.center_hz,
                    level_db: finite_or(smoothed_db, self.config.min_db),
                    normalized_level: finite_or(smoothed, 0.0).clamp(0.0, 1.0),
                }
            })
            .collect();

        self.frame_index = self.frame_index.wrapping_add(1);
        Ok(SpectrumSnapshot {
            sample_rate: self.sample_rate,
            frame_index: self.frame_index,
            bands,
        })
    }
}

fn validate_config(sample_rate: u32, config: &DspConfig) -> Result<(), DspError> {
    if sample_rate == 0 {
        return Err(DspError::InvalidSampleRate);
    }
    if config.fft_size < 4 {
        return Err(DspError::InvalidConfig(
            "FFT size must be at least four samples".to_string(),
        ));
    }
    if config.hop_size == 0 || config.hop_size > config.fft_size {
        return Err(DspError::InvalidConfig(
            "hop size must be between one and the FFT size".to_string(),
        ));
    }
    if config.band_count == 0 {
        return Err(DspError::InvalidConfig(
            "band count must be greater than zero".to_string(),
        ));
    }
    if !config.min_frequency.is_finite()
        || !config.max_frequency.is_finite()
        || config.min_frequency <= 0.0
        || config.max_frequency <= config.min_frequency
    {
        return Err(DspError::InvalidConfig(
            "frequency range must be finite, positive, and increasing".to_string(),
        ));
    }
    if !config.min_db.is_finite() || config.min_db >= 0.0 {
        return Err(DspError::InvalidConfig(
            "minimum dB must be finite and below zero".to_string(),
        ));
    }
    if !config.attack_seconds.is_finite()
        || !config.release_seconds.is_finite()
        || config.attack_seconds <= 0.0
        || config.release_seconds <= 0.0
    {
        return Err(DspError::InvalidConfig(
            "attack and release times must be finite and positive".to_string(),
        ));
    }
    Ok(())
}

fn hann_window(size: usize) -> Vec<f32> {
    let denominator = (size - 1) as f32;
    (0..size)
        .map(|index| 0.5 * (1.0 - (2.0 * std::f32::consts::PI * index as f32 / denominator).cos()))
        .collect()
}

fn build_band_ranges(
    sample_rate: u32,
    fft_size: usize,
    min_frequency: f32,
    max_frequency: f32,
    requested_count: usize,
) -> Vec<BandRange> {
    let bin_width = sample_rate as f32 / fft_size as f32;
    let nyquist_bin = fft_size / 2;
    let nyquist = sample_rate as f32 / 2.0;
    let effective_min = min_frequency.min(nyquist).max(bin_width);
    let effective_max = max_frequency.min(nyquist).max(effective_min);

    let mut active_bins = (1..=nyquist_bin)
        .filter(|bin| {
            let frequency = *bin as f32 * bin_width;
            frequency >= effective_min && frequency <= effective_max
        })
        .collect::<Vec<_>>();

    if active_bins.is_empty() {
        active_bins.push(nyquist_bin.max(1));
    }

    let band_count = requested_count.min(active_bins.len()).max(1);
    let mut boundaries = Vec::with_capacity(band_count + 1);
    boundaries.push(0);
    for band_index in 1..band_count {
        let position = band_index as f32 / band_count as f32;
        let logarithmic_position = position.powf(1.35);
        let raw = (logarithmic_position * active_bins.len() as f32).round() as usize;
        let minimum = boundaries[band_index - 1] + 1;
        let maximum = active_bins.len() - (band_count - band_index);
        boundaries.push(raw.clamp(minimum, maximum));
    }
    boundaries.push(active_bins.len());

    boundaries
        .windows(2)
        .map(|bounds| {
            let start = active_bins[bounds[0]];
            let end = active_bins[bounds[1] - 1];
            let start_frequency = start as f32 * bin_width;
            let end_frequency = end as f32 * bin_width;
            let center_hz = (start_frequency * end_frequency).sqrt();
            BandRange {
                start_bin: start,
                end_bin: end,
                center_hz: finite_or(center_hz, start_frequency),
            }
        })
        .collect()
}

fn magnitude_to_db(
    magnitude: f32,
    bin: usize,
    fft_size: usize,
    window_gain: f32,
    min_db: f32,
) -> f32 {
    if !magnitude.is_finite() || !window_gain.is_finite() || window_gain <= 0.0 {
        return min_db;
    }

    let is_nyquist = fft_size % 2 == 0 && bin == fft_size / 2;
    let scale = if bin == 0 || is_nyquist { 1.0 } else { 2.0 };
    let amplitude = magnitude * scale / window_gain;
    if !amplitude.is_finite() {
        return min_db;
    }

    let floor_amplitude = 10.0_f32.powf(min_db / 20.0);
    (20.0 * amplitude.max(floor_amplitude).log10()).clamp(min_db, 0.0)
}

fn normalize_db(level_db: f32, min_db: f32) -> f32 {
    if !level_db.is_finite() {
        return 0.0;
    }
    ((level_db - min_db) / -min_db).clamp(0.0, 1.0)
}

fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() { value } else { fallback }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config() -> DspConfig {
        DspConfig {
            fft_size: 4096,
            hop_size: 512,
            band_count: 32,
            ..DspConfig::default()
        }
    }

    fn sine_wave(sample_rate: u32, frequency: f32, count: usize) -> Vec<f32> {
        (0..count)
            .map(|index| {
                (2.0 * std::f32::consts::PI * frequency * index as f32 / sample_rate as f32).sin()
            })
            .collect()
    }

    fn peak_band(snapshot: &SpectrumSnapshot) -> &FrequencyBand {
        snapshot
            .bands
            .iter()
            .max_by(|left, right| {
                left.normalized_level
                    .partial_cmp(&right.normalized_level)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .expect("bands are not empty")
    }

    #[test]
    fn silence_is_finite_and_at_display_floor() {
        let mut analyzer = Analyzer::new(48_000, test_config()).expect("valid analyzer");
        let snapshot = analyzer
            .process(&vec![0.0; 4096])
            .expect("silence should process")
            .expect("a complete window should produce a snapshot");

        assert!(snapshot.bands.iter().all(|band| {
            band.level_db.is_finite()
                && band.normalized_level.is_finite()
                && band.level_db == -80.0
                && band.normalized_level == 0.0
        }));
    }

    #[test]
    fn very_low_amplitude_is_finite_and_at_display_floor() {
        let mut analyzer = Analyzer::new(48_000, test_config()).expect("valid analyzer");
        let input = sine_wave(48_000, 440.0, 4096)
            .into_iter()
            .map(|sample| sample * 1.0e-8)
            .collect::<Vec<_>>();
        let snapshot = analyzer
            .process(&input)
            .expect("low amplitude should process")
            .expect("complete window");

        assert!(snapshot.bands.iter().all(|band| {
            band.level_db.is_finite()
                && band.normalized_level.is_finite()
                && band.level_db == -80.0
                && band.normalized_level == 0.0
        }));
    }

    #[test]
    fn aligned_sine_peak_maps_to_expected_band() {
        let mut analyzer = Analyzer::new(48_000, test_config()).expect("valid analyzer");
        let frequency = 440.0;
        let snapshot = analyzer
            .process(&sine_wave(48_000, frequency, 4096 * 4))
            .expect("sine should process")
            .expect("a complete window should produce a snapshot");
        let peak = peak_band(&snapshot);

        assert!((300.0..=650.0).contains(&peak.center_hz));
        assert!(peak.normalized_level > 0.5);
    }

    #[test]
    fn multiple_tones_remain_visible_in_separate_bands() {
        let sample_rate = 48_000;
        let mut analyzer = Analyzer::new(sample_rate, test_config()).expect("valid analyzer");
        let input = (0..4096 * 4)
            .map(|index| {
                let time = index as f32 / sample_rate as f32;
                let low = (2.0 * std::f32::consts::PI * 220.0 * time).sin();
                let high = 0.5 * (2.0 * std::f32::consts::PI * 4_000.0 * time).sin();
                low + high
            })
            .collect::<Vec<_>>();
        let snapshot = analyzer
            .process(&input)
            .expect("multiple tones should process")
            .expect("complete window");

        let low_level = snapshot
            .bands
            .iter()
            .filter(|band| (150.0..=350.0).contains(&band.center_hz))
            .map(|band| band.normalized_level)
            .fold(0.0, f32::max);
        let high_level = snapshot
            .bands
            .iter()
            .filter(|band| (3_000.0..=5_000.0).contains(&band.center_hz))
            .map(|band| band.normalized_level)
            .fold(0.0, f32::max);

        assert!(low_level > 0.5);
        assert!(high_level > 0.3);
    }

    #[test]
    fn stereo_samples_are_averaged_to_mono() {
        let input = [1.0, -1.0, 0.25, 0.75, 0.0, 0.5];
        let mut output = Vec::new();

        let frames = mix_interleaved_to_mono(&input, 2, &mut output).expect("valid channels");

        assert_eq!(frames, 3);
        assert_eq!(output, vec![0.0, 0.5, 0.25]);
    }

    #[test]
    fn sample_rate_mapping_works_at_44k_48k_and_96k() {
        for sample_rate in [44_100, 48_000, 96_000] {
            let mut analyzer = Analyzer::new(sample_rate, test_config()).expect("valid analyzer");
            let snapshot = analyzer
                .process(&sine_wave(sample_rate, 1_000.0, 4096 * 4))
                .expect("sine should process")
                .expect("complete window");
            let peak = peak_band(&snapshot);

            assert!((700.0..=1_400.0).contains(&peak.center_hz));
        }
    }

    #[test]
    fn band_ranges_are_non_empty_and_do_not_repeat_a_bin() {
        for sample_rate in [44_100, 48_000, 96_000] {
            let analyzer = Analyzer::new(sample_rate, test_config()).expect("valid analyzer");
            let ranges = analyzer.band_ranges();

            assert!(
                ranges
                    .iter()
                    .all(|range| { range.start_bin <= range.end_bin && range.bin_count() > 0 })
            );
            assert!(
                ranges
                    .windows(2)
                    .all(|pair| pair[0].end_bin < pair[1].start_bin)
            );
        }
    }

    #[test]
    fn amplitude_normalization_is_stable_when_window_size_changes() {
        let sample_rate = 48_000;
        let mut levels = Vec::new();
        for fft_size in [2048, 4096] {
            let config = DspConfig {
                fft_size,
                hop_size: fft_size,
                band_count: 32,
                ..DspConfig::default()
            };
            let mut analyzer = Analyzer::new(sample_rate, config).expect("valid analyzer");
            let frequency = sample_rate as f32 * 32.0 / fft_size as f32;
            let snapshot = analyzer
                .process(&sine_wave(sample_rate, frequency, fft_size * 4))
                .expect("sine should process")
                .expect("complete window");
            levels.push(peak_band(&snapshot).normalized_level);
        }

        assert!(levels.iter().all(|level| *level > 0.9));
        assert!((levels[0] - levels[1]).abs() < 0.03);
    }

    #[test]
    fn incomplete_and_very_small_windows_wait_for_more_samples() {
        let mut analyzer = Analyzer::new(48_000, test_config()).expect("valid analyzer");
        assert!(
            analyzer
                .process(&[0.0; 3])
                .expect("small input should be accepted")
                .is_none()
        );
        assert!(
            analyzer
                .process(&vec![0.0; 4093])
                .expect("partial input should be accepted")
                .is_some()
        );
    }

    #[test]
    fn analyzer_reset_discards_samples_from_before_a_discontinuity() {
        let mut analyzer = Analyzer::new(48_000, test_config()).expect("valid analyzer");
        assert!(
            analyzer
                .process(&vec![0.0; 2048])
                .expect("partial input")
                .is_none()
        );

        analyzer.reset();

        assert!(
            analyzer
                .process(&vec![0.0; 2048])
                .expect("reset input")
                .is_none()
        );
        assert!(
            analyzer
                .process(&vec![0.0; 2048])
                .expect("complete post-reset input")
                .is_some()
        );
    }

    #[test]
    fn non_finite_input_never_reaches_the_snapshot() {
        let mut analyzer = Analyzer::new(48_000, test_config()).expect("valid analyzer");
        let input = vec![f32::NAN; 4096];
        let snapshot = analyzer
            .process(&input)
            .expect("non-finite samples should be sanitized")
            .expect("complete window");

        assert!(
            snapshot
                .bands
                .iter()
                .all(|band| band.level_db.is_finite() && band.normalized_level.is_finite())
        );
    }

    #[test]
    fn attack_and_release_follow_their_expected_directions() {
        let mut smoother = AttackRelease::new(0.05, 0.2).expect("valid smoother");
        let rising = smoother.advance(1.0, 0.05);
        let falling = smoother.advance(0.0, 0.05);

        assert!(rising > 0.0);
        assert!(falling < rising);
        assert!(falling > 0.0);
    }

    #[test]
    fn dynamics_profiles_provide_the_documented_time_constants() {
        assert_eq!(DynamicsProfile::Soft.attack_seconds(), 0.045);
        assert_eq!(DynamicsProfile::Soft.release_seconds(), 0.22);
        assert_eq!(DynamicsProfile::Balanced.attack_seconds(), 0.025);
        assert_eq!(DynamicsProfile::Balanced.release_seconds(), 0.15);
        assert_eq!(DynamicsProfile::Punchy.attack_seconds(), 0.015);
        assert_eq!(DynamicsProfile::Punchy.release_seconds(), 0.12);
        assert_eq!(DspConfig::default().attack_seconds, 0.015);
        assert_eq!(DspConfig::default().release_seconds, 0.12);

        let balanced = DspConfig::default().with_dynamics(DynamicsProfile::Balanced);
        assert_eq!(balanced.attack_seconds, 0.025);
        assert_eq!(balanced.release_seconds, 0.15);
    }

    #[test]
    fn smoothing_uses_elapsed_time_not_renderer_frame_count() {
        let mut one_step = AttackRelease::new(0.05, 0.2).expect("valid smoother");
        let mut ten_steps = AttackRelease::new(0.05, 0.2).expect("valid smoother");
        let one_step_value = one_step.advance(1.0, 0.1);
        for _ in 0..10 {
            let _ = ten_steps.advance(1.0, 0.01);
        }

        assert!((one_step_value - ten_steps.value()).abs() < 0.01);
    }

    #[test]
    fn analyzer_can_be_rebuilt_for_a_new_format() {
        let mut analyzer = Analyzer::new(48_000, test_config()).expect("valid analyzer");
        let _ = analyzer.process(&vec![0.0; 4096]);
        analyzer = Analyzer::new(44_100, analyzer.config().clone()).expect("new format");

        assert_eq!(analyzer.sample_rate(), 44_100);
        assert!(analyzer.process(&vec![0.0; 4096]).is_ok());
    }
}

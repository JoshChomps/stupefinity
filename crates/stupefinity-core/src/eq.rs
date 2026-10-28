use librespot::playback::audio_backend::{Sink, SinkResult};
use librespot::playback::convert::Converter;
use librespot::playback::decoder::AudioPacket;
use rodio::source::SeekError;
use rodio::Source;
use std::f32::consts::PI;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use stupefinity_protocol::{EqProfile, FilterBand};
use tracing::info;

/// Implements a Direct Form II Transposed biquad filter.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct BiquadFilter {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    // State memory for 2 channels (Left, Right)
    s1: [f32; 2],
    s2: [f32; 2],
}

impl BiquadFilter {
    pub fn new(band: &FilterBand, sample_rate: f32) -> Self {
        let f0 = band.frequency;
        let q = band.q.max(0.01);
        let a = 10.0f32.powf(band.gain_db / 40.0);
        let omega = 2.0 * PI * f0 / sample_rate;
        let sin_omega = omega.sin();
        let cos_omega = omega.cos();
        let alpha = sin_omega / (2.0 * q);

        let (b0, b1, b2, a0, a1, a2) = match band.filter_type.to_lowercase().as_str() {
            "lowshelf" => {
                let a_plus_1 = a + 1.0;
                let a_minus_1 = a - 1.0;
                let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;

                let b0 = a * (a_plus_1 - a_minus_1 * cos_omega + two_sqrt_a_alpha);
                let b1 = 2.0 * a * (a_minus_1 - a_plus_1 * cos_omega);
                let b2 = a * (a_plus_1 - a_minus_1 * cos_omega - two_sqrt_a_alpha);
                let a0 = a_plus_1 + a_minus_1 * cos_omega + two_sqrt_a_alpha;
                let a1 = -2.0 * (a_minus_1 + a_plus_1 * cos_omega);
                let a2 = a_plus_1 + a_minus_1 * cos_omega - two_sqrt_a_alpha;
                (b0, b1, b2, a0, a1, a2)
            }
            "highshelf" => {
                let a_plus_1 = a + 1.0;
                let a_minus_1 = a - 1.0;
                let two_sqrt_a_alpha = 2.0 * a.sqrt() * alpha;

                let b0 = a * (a_plus_1 + a_minus_1 * cos_omega + two_sqrt_a_alpha);
                let b1 = -2.0 * a * (a_minus_1 + a_plus_1 * cos_omega);
                let b2 = a * (a_plus_1 + a_minus_1 * cos_omega - two_sqrt_a_alpha);
                let a0 = a_plus_1 - a_minus_1 * cos_omega + two_sqrt_a_alpha;
                let a1 = 2.0 * (a_minus_1 - a_plus_1 * cos_omega);
                let a2 = a_plus_1 - a_minus_1 * cos_omega - two_sqrt_a_alpha;
                (b0, b1, b2, a0, a1, a2)
            }
            "lowpass" => {
                let b0 = (1.0 - cos_omega) / 2.0;
                let b1 = 1.0 - cos_omega;
                let b2 = (1.0 - cos_omega) / 2.0;
                let a0 = 1.0 + alpha;
                let a1 = -2.0 * cos_omega;
                let a2 = 1.0 - alpha;
                (b0, b1, b2, a0, a1, a2)
            }
            "highpass" => {
                let b0 = (1.0 + cos_omega) / 2.0;
                let b1 = -(1.0 + cos_omega);
                let b2 = (1.0 + cos_omega) / 2.0;
                let a0 = 1.0 + alpha;
                let a1 = -2.0 * cos_omega;
                let a2 = 1.0 - alpha;
                (b0, b1, b2, a0, a1, a2)
            }
            _ => {
                let b0 = 1.0 + alpha * a;
                let b1 = -2.0 * cos_omega;
                let b2 = 1.0 - alpha * a;
                let a0 = 1.0 + alpha / a;
                let a1 = -2.0 * cos_omega;
                let a2 = 1.0 - alpha / a;
                (b0, b1, b2, a0, a1, a2)
            }
        };

        Self {
            b0: b0 / a0,
            b1: b1 / a0,
            b2: b2 / a0,
            a1: a1 / a0,
            a2: a2 / a0,
            s1: [0.0, 0.0],
            s2: [0.0, 0.0],
        }
    }

    /// Process a stereo sample [L, R] through the filter.
    #[inline]
    pub fn process_sample(&mut self, ch: usize, input: f32) -> f32 {
        let out = self.b0 * input + self.s1[ch];
        self.s1[ch] = self.b1 * input - self.a1 * out + self.s2[ch];
        self.s2[ch] = self.b2 * input - self.a2 * out;
        out
    }

    /// Clears the filter memory, e.g. after a seek, so old audio does not ring into new.
    pub fn reset(&mut self) {
        self.s1 = [0.0; 2];
        self.s2 = [0.0; 2];
    }
}

static SHARED: OnceLock<Arc<Mutex<EqualizerEngine>>> = OnceLock::new();

/// The app-wide equalizer, shared by the local audio player and the librespot output.
pub fn shared() -> Arc<Mutex<EqualizerEngine>> {
    SHARED
        .get_or_init(|| Arc::new(Mutex::new(EqualizerEngine::new(44100.0))))
        .clone()
}

/// Headphone Equalizer DSP Engine.
pub struct EqualizerEngine {
    profile: EqProfile,
    filters: Vec<BiquadFilter>,
    preamp_linear: f32,
    sample_rate: f32,
}

impl EqualizerEngine {
    pub fn new(sample_rate: f32) -> Self {
        Self {
            profile: EqProfile::default(),
            filters: Vec::new(),
            preamp_linear: 1.0,
            sample_rate,
        }
    }

    /// Load an AutoEq profile. Preamp is scaled first to avoid digital clipping.
    /// The current enabled/bypass state is kept; use `set_enabled` to change it.
    pub fn set_profile(&mut self, mut profile: EqProfile) {
        profile.enabled = self.profile.enabled;
        info!(
            "Loading AutoEq profile: '{}' (Preamp: {} dB, {} bands)",
            profile.name,
            profile.preamp_db,
            profile.bands.len()
        );
        self.preamp_linear = 10.0f32.powf(profile.preamp_db / 20.0);
        self.filters = profile
            .bands
            .iter()
            .map(|b| BiquadFilter::new(b, self.sample_rate))
            .collect();
        self.profile = profile;
    }

    /// Toggle EQ bypass on/off.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.profile.enabled = enabled;
        info!("Equalizer enabled: {}", enabled);
    }

    /// Check if the equalizer is currently active with configured filters.
    pub fn is_enabled(&self) -> bool {
        self.profile.enabled && !self.filters.is_empty()
    }

    /// Rebuild filters if sample rate changes (e.g. 44.1 kHz vs 48 kHz).
    pub fn set_sample_rate(&mut self, sample_rate: f32) {
        if (self.sample_rate - sample_rate).abs() > 1.0 {
            self.sample_rate = sample_rate;
            self.filters = self
                .profile
                .bands
                .iter()
                .map(|b| BiquadFilter::new(b, self.sample_rate))
                .collect();
        }
    }

    /// Clears every filter's memory (call after a seek or track change).
    pub fn reset_state(&mut self) {
        self.filters.iter_mut().for_each(BiquadFilter::reset);
    }

    /// In-place DSP processing of mono f32 PCM audio samples.
    pub fn process_mono(&mut self, samples: &mut [f32]) {
        if !self.is_enabled() {
            return;
        }

        for s in samples {
            let mut val = *s * self.preamp_linear;
            for filter in &mut self.filters {
                val = filter.process_sample(0, val);
            }
            *s = val.clamp(-1.0, 1.0);
        }
    }

    /// In-place DSP processing of stereo interleaved f32 PCM audio samples.
    pub fn process_interleaved(&mut self, samples: &mut [f32]) {
        if !self.is_enabled() {
            return;
        }

        for chunk in samples.as_chunks_mut::<2>().0 {
            // Apply negative preamp first
            let mut left = chunk[0] * self.preamp_linear;
            let mut right = chunk[1] * self.preamp_linear;

            // Cascade through all biquad filters
            for filter in &mut self.filters {
                left = filter.process_sample(0, left);
                right = filter.process_sample(1, right);
            }

            chunk[0] = left.clamp(-1.0, 1.0);
            chunk[1] = right.clamp(-1.0, 1.0);
        }
    }

    /// Calculate equal-power crossfade gain multipliers for outgoing and incoming tracks.
    /// `progress` is from 0.0 to 1.0.
    #[allow(dead_code)]
    #[inline]
    pub fn equal_power_crossfade(progress: f32) -> (f32, f32) {
        let t = progress.clamp(0.0, 1.0);
        let out_gain = (t * PI * 0.5).cos();
        let in_gain = (t * PI * 0.5).sin();
        (out_gain, in_gain)
    }
}

/// A rodio `Source` filter that processes audio through `EqualizerEngine` in real time.
pub struct EqualizerSource<S> {
    input: S,
    eq: Arc<Mutex<EqualizerEngine>>,
    buffer: Vec<f32>,
    buf_pos: usize,
    channels: u16,
    sample_rate: u32,
}

impl<S> EqualizerSource<S>
where
    S: Source<Item = f32>,
{
    pub fn new(input: S, eq: Arc<Mutex<EqualizerEngine>>) -> Self {
        let channels = input.channels();
        let sample_rate = input.sample_rate();
        if let Ok(mut engine) = eq.lock() {
            engine.set_sample_rate(sample_rate as f32);
        }
        Self {
            input,
            eq,
            buffer: Vec::with_capacity(512),
            buf_pos: 0,
            channels,
            sample_rate,
        }
    }

    fn fill_buffer(&mut self) -> bool {
        self.buffer.clear();
        self.buf_pos = 0;
        let target_len = (self.channels as usize).max(1) * 256;
        for _ in 0..target_len {
            if let Some(s) = self.input.next() {
                self.buffer.push(s);
            } else {
                break;
            }
        }

        if self.buffer.is_empty() {
            return false;
        }

        if let Ok(mut engine) = self.eq.lock() {
            engine.set_sample_rate(self.sample_rate as f32);
            if engine.is_enabled() {
                if self.channels == 2 {
                    engine.process_interleaved(&mut self.buffer);
                } else if self.channels == 1 {
                    engine.process_mono(&mut self.buffer);
                }
            }
        }
        true
    }
}

impl<S> Iterator for EqualizerSource<S>
where
    S: Source<Item = f32>,
{
    type Item = f32;

    #[inline]
    fn next(&mut self) -> Option<f32> {
        if self.buf_pos >= self.buffer.len() && !self.fill_buffer() {
            return None;
        }
        let sample = self.buffer[self.buf_pos];
        self.buf_pos += 1;
        Some(sample)
    }
}

impl<S> Source for EqualizerSource<S>
where
    S: Source<Item = f32>,
{
    fn current_span_len(&self) -> Option<usize> {
        let remaining = self.buffer.len() - self.buf_pos;
        if remaining > 0 {
            Some(remaining)
        } else {
            self.input.current_span_len()
        }
    }

    fn channels(&self) -> u16 {
        self.channels
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn total_duration(&self) -> Option<Duration> {
        self.input.total_duration()
    }

    fn try_seek(&mut self, pos: Duration) -> Result<(), SeekError> {
        self.input.try_seek(pos)?;
        // Drop audio already filtered from before the seek point.
        self.buffer.clear();
        self.buf_pos = 0;
        if let Ok(mut engine) = self.eq.lock() {
            engine.reset_state();
        }
        Ok(())
    }
}

/// Runs librespot's decoded audio (always 44.1 kHz stereo) through the equalizer before
/// handing it to librespot's own output sink, so Premium playback gets the EQ too.
pub struct EqSink {
    inner: Box<dyn Sink>,
    eq: Arc<Mutex<EqualizerEngine>>,
    buffer: Vec<f32>,
}

impl EqSink {
    pub fn new(inner: Box<dyn Sink>, eq: Arc<Mutex<EqualizerEngine>>) -> Self {
        Self {
            inner,
            eq,
            buffer: Vec::new(),
        }
    }
}

impl Sink for EqSink {
    fn start(&mut self) -> SinkResult<()> {
        self.inner.start()
    }

    fn stop(&mut self) -> SinkResult<()> {
        if let Ok(mut engine) = self.eq.lock() {
            engine.reset_state();
        }
        self.inner.stop()
    }

    fn write(&mut self, mut packet: AudioPacket, converter: &mut Converter) -> SinkResult<()> {
        if let AudioPacket::Samples(samples) = &mut packet {
            if let Ok(mut engine) = self.eq.lock() {
                if engine.is_enabled() {
                    engine.set_sample_rate(44100.0);
                    self.buffer.clear();
                    self.buffer.extend(samples.iter().map(|&s| s as f32));
                    engine.process_interleaved(&mut self.buffer);
                    for (dst, &src) in samples.iter_mut().zip(&self.buffer) {
                        *dst = f64::from(src);
                    }
                }
            }
        }
        self.inner.write(packet, converter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Magnitude response of a filter at DC (z = 1) and Nyquist (z = -1), in dB.
    fn dc_and_nyquist_db(f: &BiquadFilter) -> (f32, f32) {
        let dc = (f.b0 + f.b1 + f.b2) / (1.0 + f.a1 + f.a2);
        let ny = (f.b0 - f.b1 + f.b2) / (1.0 - f.a1 + f.a2);
        (20.0 * dc.abs().log10(), 20.0 * ny.abs().log10())
    }

    fn band(filter_type: &str, gain_db: f32) -> FilterBand {
        FilterBand {
            filter_type: filter_type.into(),
            frequency: 1000.0,
            q: 0.71,
            gain_db,
        }
    }

    #[test]
    fn shelf_gains_land_on_the_correct_side() {
        let (dc, ny) = dc_and_nyquist_db(&BiquadFilter::new(&band("LowShelf", 6.0), 44100.0));
        assert!(
            (dc - 6.0).abs() < 0.05 && ny.abs() < 0.05,
            "lowshelf dc={dc} ny={ny}"
        );

        let (dc, ny) = dc_and_nyquist_db(&BiquadFilter::new(&band("HighShelf", 6.0), 44100.0));
        assert!(
            dc.abs() < 0.05 && (ny - 6.0).abs() < 0.05,
            "highshelf dc={dc} ny={ny}"
        );
    }

    #[test]
    fn peaking_is_transparent_away_from_center() {
        let (dc, ny) = dc_and_nyquist_db(&BiquadFilter::new(&band("Peaking", 6.0), 44100.0));
        assert!(
            dc.abs() < 0.05 && ny.abs() < 0.05,
            "peaking dc={dc} ny={ny}"
        );
    }

    struct MockSource {
        samples: Vec<f32>,
        pos: usize,
    }

    impl Iterator for MockSource {
        type Item = f32;
        fn next(&mut self) -> Option<f32> {
            if self.pos < self.samples.len() {
                let s = self.samples[self.pos];
                self.pos += 1;
                Some(s)
            } else {
                None
            }
        }
    }

    impl rodio::Source for MockSource {
        fn current_span_len(&self) -> Option<usize> {
            None
        }
        fn channels(&self) -> u16 {
            2
        }
        fn sample_rate(&self) -> u32 {
            44100
        }
        fn total_duration(&self) -> Option<Duration> {
            None
        }
    }

    #[test]
    fn equalizer_source_processes_stream() {
        let mut engine = EqualizerEngine::new(44100.0);
        engine.set_profile(EqProfile {
            name: "Test".into(),
            preamp_db: -6.0,
            bands: vec![band("LowShelf", 6.0)],
            enabled: true,
        });
        engine.set_enabled(true);

        let eq = Arc::new(Mutex::new(engine));
        let input = MockSource {
            samples: vec![0.5; 100],
            pos: 0,
        };
        let mut eq_source = EqualizerSource::new(input, eq);

        let mut output = Vec::new();
        for s in &mut eq_source {
            output.push(s);
        }
        assert_eq!(output.len(), 100);
        // Preamp is -6dB (~0.501) and low shelf gain applies, so output won't be 0.5
        assert!((output[0] - 0.5).abs() > 0.01);
    }
}

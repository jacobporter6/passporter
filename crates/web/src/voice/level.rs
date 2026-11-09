//! Microphone loudness: what the live meter draws, and what silence
//! auto-stop listens to. Pure, so it is tested on the host.

use std::collections::VecDeque;

/// Bars in the live meter, newest last.
pub const METER_BARS: usize = 24;

/// The quietest level the meter shows above zero, in dBFS.
const METER_FLOOR_DB: f32 = -54.0;
/// The level that fills a bar, in dBFS (speech near the phone peaks here).
const METER_CEIL_DB: f32 = -12.0;

/// Loudness of one analyser frame of unsigned 8-bit samples (128 is
/// silence, as `AnalyserNode.getByteTimeDomainData` writes them), as the
/// root mean square in `0.0..=1.0`.
#[must_use]
pub fn rms_u8(samples: &[u8]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f32 = samples
        .iter()
        .map(|&sample| {
            let centred = (f32::from(sample) - 128.0) / 128.0;
            centred * centred
        })
        .sum();
    // An analyser frame is at most 32768 samples: exact in an f32.
    #[allow(clippy::cast_precision_loss)]
    let count = samples.len() as f32;
    (sum / count).sqrt().min(1.0)
}

/// How tall a bar is for `rms`, in `0.0..=1.0`: logarithmic, like hearing,
/// so quiet speech still moves the meter and shouting doesn't clip it all.
#[must_use]
pub fn bar_height(rms: f32) -> f32 {
    if rms <= 0.0 {
        return 0.0;
    }
    let db = 20.0 * rms.log10();
    ((db - METER_FLOOR_DB) / (METER_CEIL_DB - METER_FLOOR_DB)).clamp(0.0, 1.0)
}

/// The rolling bars of the live meter.
#[derive(Debug, Clone, PartialEq)]
pub struct LevelMeter {
    bars: VecDeque<f32>,
}

impl Default for LevelMeter {
    fn default() -> Self {
        Self {
            bars: std::iter::repeat_n(0.0, METER_BARS).collect(),
        }
    }
}

impl LevelMeter {
    /// Adds the newest frame's loudness, dropping the oldest bar.
    pub fn push(&mut self, rms: f32) {
        if self.bars.len() == METER_BARS {
            self.bars.pop_front();
        }
        self.bars.push_back(bar_height(rms));
    }

    /// Bar heights in `0.0..=1.0`, oldest first; always [`METER_BARS`].
    #[must_use]
    pub fn bars(&self) -> Vec<f32> {
        self.bars.iter().copied().collect()
    }
}

/// When to stop on silence (design §6.3: configurable).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SilenceConfig {
    /// Frames quieter than this RMS count as silence. Room noise on a
    /// phone sits around 0.005–0.02; speech is well above 0.03.
    pub threshold: f32,
    /// Stop once it has been quiet this long after speech, in ms.
    pub after_ms: f64,
}

impl Default for SilenceConfig {
    fn default() -> Self {
        Self {
            threshold: 0.03,
            after_ms: 2_500.0,
        }
    }
}

/// Decides when a recording has gone quiet. Silence only counts once the
/// user has said something, so a slow start is never cut off.
#[derive(Debug, Clone, PartialEq)]
pub struct SilenceGate {
    config: SilenceConfig,
    heard_speech: bool,
    quiet_since: Option<f64>,
}

impl SilenceGate {
    /// A gate that has heard nothing yet.
    #[must_use]
    pub const fn new(config: SilenceConfig) -> Self {
        Self {
            config,
            heard_speech: false,
            quiet_since: None,
        }
    }

    /// Feeds one frame's loudness at `at_ms`; true once the recording
    /// should stop.
    pub fn hear(&mut self, at_ms: f64, rms: f32) -> bool {
        if rms >= self.config.threshold {
            self.heard_speech = true;
            self.quiet_since = None;
            return false;
        }
        if !self.heard_speech {
            return false;
        }
        let since = *self.quiet_since.get_or_insert(at_ms);
        at_ms - since >= self.config.after_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rms_of_silence_and_full_scale() {
        assert!(rms_u8(&[]).abs() < f32::EPSILON);
        assert!(rms_u8(&[128; 64]).abs() < f32::EPSILON);
        let square: Vec<u8> = (0..64).map(|i| if i % 2 == 0 { 0 } else { 255 }).collect();
        assert!(rms_u8(&square) > 0.99, "{}", rms_u8(&square));
    }

    #[test]
    fn bars_are_logarithmic_and_clamped() {
        assert!(bar_height(0.0).abs() < f32::EPSILON);
        assert!(bar_height(0.001).abs() < f32::EPSILON, "below the floor");
        assert!((bar_height(1.0) - 1.0).abs() < f32::EPSILON, "clipped");
        let quiet = bar_height(0.01);
        let loud = bar_height(0.1);
        assert!(0.0 < quiet && quiet < loud && loud < 1.0, "{quiet} {loud}");
    }

    #[test]
    fn the_meter_rolls() {
        let mut meter = LevelMeter::default();
        assert_eq!(meter.bars().len(), METER_BARS);
        meter.push(1.0);
        for _ in 1..METER_BARS {
            meter.push(0.0);
        }
        assert!((meter.bars()[0] - 1.0).abs() < f32::EPSILON);
        meter.push(0.0);
        assert!(meter.bars().iter().all(|bar| bar.abs() < f32::EPSILON));
        assert_eq!(meter.bars().len(), METER_BARS);
    }

    #[test]
    fn silence_counts_only_after_speech() {
        let mut gate = SilenceGate::new(SilenceConfig {
            threshold: 0.05,
            after_ms: 1_000.0,
        });
        // A long quiet start never stops.
        assert!(!gate.hear(0.0, 0.0));
        assert!(!gate.hear(5_000.0, 0.01));
        assert!(!gate.hear(5_100.0, 0.2));
        assert!(!gate.hear(5_500.0, 0.0));
        // Speech again resets the clock.
        assert!(!gate.hear(6_000.0, 0.2));
        assert!(!gate.hear(6_100.0, 0.0));
        assert!(!gate.hear(7_000.0, 0.0));
        assert!(gate.hear(7_100.0, 0.0));
    }
}

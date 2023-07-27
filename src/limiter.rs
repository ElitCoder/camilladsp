use circular_queue::CircularQueue;

use crate::basicfilters::Delay;
use crate::config;
use crate::filters::Filter;
use crate::PrcFmt;
use crate::Res;

const CUBEFACTOR: PrcFmt = 1.0 / 6.75; // = 1 / (2 * 1.5^3)

#[derive(Clone, Debug)]
pub struct Limiter {
    pub name: String,
    pub soft_clip: bool,
    pub clip_limit: PrcFmt,
    pub sample_rate: usize,
    pub delay: Delay,
    pub lookahead: usize,
    pub clip_control_history: CircularQueue<PrcFmt>,
    pub prev_peak: PrcFmt,
    pub alpha: PrcFmt,
    pub beta: PrcFmt,
}

impl Limiter {
    /// Creates a Compressor from a config struct
    pub fn from_config(name: &str, sample_rate: usize, config: config::LimiterParameters) -> Self {
        let clip_limit = (10.0 as PrcFmt).powf(config.clip_limit / 20.0);
        let lookahead = config.lookahead.unwrap_or_default();

        let (alpha, beta, delay) = calculate_lookahead_parameters(lookahead, sample_rate);

        debug!(
            "Creating limiter '{}', soft_clip: {}, clip_limit dB: {}, linear: {}, lookahead: {} samples, alpha: {}, beta: {}",
            name,
            config.soft_clip(),
            config.clip_limit,
            clip_limit,
            lookahead,
            alpha,
            beta,
        );

        Limiter {
            name: name.to_string(),
            soft_clip: config.soft_clip(),
            clip_limit,
            sample_rate,
            delay: delay,
            lookahead,
            clip_control_history: CircularQueue::with_capacity(lookahead),
            prev_peak: 0.0,
            alpha,
            beta,
        }
    }

    fn apply_soft_clip(&self, input: &mut [PrcFmt]) {
        for val in input.iter_mut() {
            let mut scaled = *val / self.clip_limit;
            scaled = scaled.clamp(-1.5, 1.5);
            scaled -= CUBEFACTOR * scaled.powi(3);
            *val = scaled * self.clip_limit;
        }
    }

    fn apply_hard_clip(&self, input: &mut [PrcFmt]) {
        for val in input.iter_mut() {
            *val = val.clamp(-self.clip_limit, self.clip_limit);
        }
    }

    pub fn apply_clip(&self, input: &mut [PrcFmt]) {
        if self.soft_clip {
            self.apply_soft_clip(input);
        } else {
            self.apply_hard_clip(input);
        }
    }

    pub fn calculate_gain(&mut self, waveform: &[PrcFmt]) -> Vec<PrcFmt> {
        waveform
            .iter()
            .map(|x| {
                // Calculate the incoming peak values
                let sample = x.abs();
                let sample_overshoot = (sample - self.beta * self.prev_peak) / (1.0 - self.beta);
                let clipping_control = sample.max(sample_overshoot);

                // Update the clipping block history
                self.clip_control_history.push(clipping_control);

                // Run the max filter and update the envelope
                let max_sample = self
                    .clip_control_history
                    .asc_iter()
                    .fold(0.0, |max, x| x.max(max));
                self.prev_peak = self.alpha * max_sample + (1.0 - self.alpha) * self.prev_peak;
                if self.prev_peak > self.clip_limit {
                    self.clip_limit / self.prev_peak
                } else {
                    1.0 // No gain
                }
            })
            .collect()
    }

    pub fn apply_limiter(&self, gains: Vec<PrcFmt>, waveform: &mut [PrcFmt]) {
        waveform
            .iter_mut()
            .zip(gains)
            .for_each(|(sample, gain)| *sample *= gain);
    }
}

impl Filter for Limiter {
    fn name(&self) -> &str {
        &self.name
    }

    /// Apply a Compressor to an AudioChunk, modifying it in-place.
    fn process_waveform(&mut self, waveform: &mut [PrcFmt]) -> Res<()> {
        if self.lookahead > 0 {
            // Calculate gain from incoming samples
            let gains = self.calculate_gain(waveform);
            // Apply delay to the playback signal and then apply the gain
            self.delay.process_waveform(waveform).unwrap();
            self.apply_limiter(gains, waveform);
        } else {
            // Otherwise use normal soft or hard clipping
            self.apply_clip(waveform);
        }
        Ok(())
    }

    fn update_parameters(&mut self, config: config::Filter) {
        if let config::Filter::Limiter {
            parameters: config, ..
        } = config
        {
            let clip_limit = (10.0 as PrcFmt).powf(config.clip_limit / 20.0);
            let lookahead = config.lookahead();
            let (alpha, beta, delay) = calculate_lookahead_parameters(lookahead, self.sample_rate);

            self.delay = delay;
            self.lookahead = lookahead;
            self.alpha = alpha;
            self.beta = beta;
            self.soft_clip = config.soft_clip();
            self.clip_limit = clip_limit;
            debug!(
                "Updated limiter '{}', soft_clip: {}, clip_limit dB: {}, linear: {}, lookahead: {} samples, alpha: {}, beta: {}",
                self.name,
                config.soft_clip(),
                config.clip_limit,
                clip_limit,
                lookahead,
                alpha,
                beta,
            );
        } else {
            // This should never happen unless there is a bug somewhere else
            panic!("Invalid config change!");
        }
    }
}

/// Validate the limiter config, always return ok to allow any config.
pub fn validate_config(_config: &config::LimiterParameters) -> Res<()> {
    Ok(())
}

/// Calculate common parameters when creating and updating the limiter configration
fn calculate_lookahead_parameters(lookahead: usize, sample_rate: usize) -> (PrcFmt, PrcFmt, Delay) {
    // Create delay
    let delay_conf = config::DelayParameters {
        delay: lookahead as PrcFmt,
        unit: Some(config::TimeUnit::Samples),
        subsample: None,
    };
    let delay = Delay::from_config("LimiterDelay", sample_rate, delay_conf);

    // Calculate alpha and beta
    let overshoot = 1.01 as PrcFmt; // Needed overshoot to avoid clipping
    let alpha = 1.0
        - (10.0 as PrcFmt)
            .powf(((overshoot - 1.0) / overshoot).log10() / ((lookahead as PrcFmt) + 1.0));
    let beta = (1.0 - alpha).powi((lookahead as i32) + 1);

    (alpha, beta, delay)
}

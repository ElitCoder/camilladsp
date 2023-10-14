use crate::audiodevice::AudioChunk;
use crate::config;
use crate::filters::Processor;
use crate::limiter::Limiter;
use crate::PrcFmt;
use crate::Res;

#[derive(Clone, Debug)]
pub struct Compressor {
    pub name: String,
    pub channels: usize,
    pub monitor_channels: Vec<usize>,
    pub process_channels: Vec<usize>,
    pub attack: PrcFmt,
    pub release: PrcFmt,
    pub threshold: PrcFmt,
    pub factor: PrcFmt,
    pub makeup_gain: PrcFmt,
    pub limiters: Option<Vec<Limiter>>,
    pub samplerate: usize,
    pub scratch: Vec<PrcFmt>,
    pub prev_loudness: PrcFmt,
    pub prev_gain: PrcFmt,
    pub clip_use_monitor: bool,
    pub monitor_use_power: bool,
}

impl Compressor {
    /// Creates a Compressor from a config struct
    pub fn from_config(
        name: &str,
        config: config::CompressorParameters,
        samplerate: usize,
        chunksize: usize,
    ) -> Self {
        let name = name.to_string();
        let channels = config.channels;
        let srate = samplerate as PrcFmt;
        let mut monitor_channels = config.monitor_channels();
        if monitor_channels.is_empty() {
            for n in 0..channels {
                monitor_channels.push(n);
            }
        }
        let mut process_channels = config.process_channels();
        if process_channels.is_empty() {
            for n in 0..channels {
                process_channels.push(n);
            }
        }
        let attack = (-1.0 / srate / config.attack).exp();
        let release = (-1.0 / srate / (config.release - config.attack)).exp();
        let clip_limit = config
            .clip_limit
            .map(|lim| (10.0 as PrcFmt).powf(lim / 20.0));

        let scratch = vec![0.0; chunksize];

        // Limit each playback channel by itself by default
        let clip_use_monitor = config.clip_use_monitor.unwrap_or(false);

        // Sum up monitor channels using power or voltage
        let monitor_use_power = config.monitor_use_power.unwrap_or(false);

        debug!("Creating compressor '{}', channels: {}, monitor_channels: {:?}, process_channels: {:?}, attack: {}, release: {}, threshold: {}, factor: {}, makeup_gain: {}, soft_clip: {}, clip_limit: {:?}, clip_lookahead: {}, clip_use_monitor: {}",
                name, channels, process_channels, monitor_channels, attack, release, config.threshold, config.factor, config.makeup_gain(), config.soft_clip(), clip_limit, config.clip_lookahead(), config.clip_use_monitor());
        let limiters = if let Some(limit) = config.clip_limit {
            let limitconf = config::LimiterParameters {
                clip_limit: limit,
                soft_clip: config.soft_clip,
                lookahead: config.clip_lookahead,
            };
            let limiter = Limiter::from_config("Limiter", samplerate, limitconf);
            Some(vec![limiter; process_channels.len()])
        } else {
            None
        };

        Compressor {
            name,
            channels,
            monitor_channels,
            process_channels,
            attack,
            release,
            threshold: config.threshold,
            factor: config.factor,
            makeup_gain: config.makeup_gain(),
            limiters: limiters,
            samplerate,
            scratch,
            prev_loudness: 0.0,
            prev_gain: 1.0,
            clip_use_monitor: clip_use_monitor,
            monitor_use_power: monitor_use_power,
        }
    }

    /// Sum all channels that are included in loudness monitoring, store result in self.scratch
    fn sum_monitor_channels(&mut self, input: &AudioChunk) {
        if self.monitor_channels.len() == 1 {
            let ch = self.monitor_channels[0];
            self.scratch.copy_from_slice(&input.waveforms[ch]);
        } else {
            if self.monitor_use_power {
                for (idx, _) in input.waveforms[self.monitor_channels[0]].iter().enumerate() {
                    self.scratch[idx] = self.monitor_channels.iter().fold(0.0, |acc, channel| {
                        acc + input.waveforms[self.monitor_channels[*channel]][idx].powi(2)
                    }).sqrt();
                }
                println!("HEREEEEEEEE {}", self.scratch[0]);
            } else {
                let ch = self.monitor_channels[0];
                self.scratch.copy_from_slice(&input.waveforms[ch]);
                for ch in self.monitor_channels.iter().skip(1) {
                    for (acc, val) in self.scratch.iter_mut().zip(input.waveforms[*ch].iter()) {
                        *acc += *val;
                    }
                }
            }
        }
    }

    /// Estimate loudness, store result in self.scratch
    fn estimate_loudness(&mut self) {
        for val in self.scratch.iter_mut() {
            // Calculate RMS using moving average
            self.prev_loudness =
                self.attack * self.prev_loudness + (1.0 - self.attack) * val.powi(2);
            *val = self.prev_loudness.sqrt();
        }
    }

    /// Calculate linear gain, store result in self.scratch
    fn calculate_linear_gain(&mut self) {
        let threshold_linear = (10.0 as PrcFmt).powf(self.threshold / 20.0);
        let makeup_gain_linear = (10.0 as PrcFmt).powf(self.makeup_gain / 20.0);
        for val in self.scratch.iter_mut() {
            let gain = if *val > threshold_linear {
                // FIXME: Add an option in the configuration to pick RMS compressor with limiter functionality
                if self.factor > 1000.0 {
                    // Limiter in lack of a configuration variable
                    threshold_linear / *val
                } else {
                    // Compressor
                    let rms_db = (20.0 as PrcFmt) * val.log10();
                    let gain_db = -(rms_db - self.threshold) * (self.factor - 1.0) / self.factor;
                    (10.0 as PrcFmt).powf(gain_db / 20.0)
                }
            } else {
                // FIXME: This seems to cause very long release times, investigate
                self.release * self.prev_gain + (1.0 - self.release) * 1.0
            };
            self.prev_gain = gain;
            *val = gain * makeup_gain_linear;
        }
    }

    fn apply_gain(&self, input: &mut [PrcFmt]) {
        for (val, gain) in input.iter_mut().zip(self.scratch.iter()) {
            *val *= gain;
        }
    }
}

impl Processor for Compressor {
    fn name(&self) -> &str {
        &self.name
    }

    /// Apply a Compressor to an AudioChunk, modifying it in-place.
    fn process_chunk(&mut self, input: &mut AudioChunk) -> Res<()> {
        self.sum_monitor_channels(input);
        self.estimate_loudness();
        self.calculate_linear_gain();
        for ch in self.process_channels.iter() {
            self.apply_gain(&mut input.waveforms[*ch]);
        }
        if self.clip_use_monitor {
            // Sum monitor channels again since the result is overwritten in the compressor gain calculations
            self.sum_monitor_channels(input);
        }
        if let Some(limiters) = &mut self.limiters {
            for (limiter, ch) in limiters.iter_mut().zip(self.process_channels.iter()) {
                if self.clip_use_monitor {
                    // TODO: This can be done quicker by just calculating the monitor channel gains once
                    limiter.process_limiter_with_monitor(&self.scratch, &mut input.waveforms[*ch]);
                } else {
                    limiter.process_limiter(&mut input.waveforms[*ch]);
                }
            }
        }
        Ok(())
    }

    fn update_parameters(&mut self, config: config::Processor) {
        // TODO remove when there is more than one type of Processor.
        #[allow(irrefutable_let_patterns)]
        if let config::Processor::Compressor {
            parameters: config, ..
        } = config
        {
            let channels = config.channels;
            let srate = self.samplerate as PrcFmt;
            let mut monitor_channels = config.monitor_channels();
            if monitor_channels.is_empty() {
                for n in 0..channels {
                    monitor_channels.push(n);
                }
            }
            let mut process_channels = config.process_channels();
            if process_channels.is_empty() {
                for n in 0..channels {
                    process_channels.push(n);
                }
            }
            let attack = (-1.0 / srate / config.attack).exp();
            let release = (-1.0 / srate / config.release).exp();
            let clip_limit = config
                .clip_limit
                .map(|lim| (10.0 as PrcFmt).powf(lim / 20.0));

            let limiters = if let Some(limit) = config.clip_limit {
                let limitconf = config::LimiterParameters {
                    clip_limit: limit,
                    soft_clip: config.soft_clip,
                    lookahead: config.clip_lookahead,
                };
                let limiter = Limiter::from_config("Limiter", self.samplerate, limitconf);
                Some(vec![limiter; process_channels.len()])
            } else {
                None
            };

            self.limiters = limiters;
            self.monitor_channels = monitor_channels;
            self.process_channels = process_channels;
            self.attack = attack;
            self.release = release;
            self.threshold = config.threshold;
            self.factor = config.factor;
            self.makeup_gain = config.makeup_gain();
            self.clip_use_monitor = config.clip_use_monitor();
            self.monitor_use_power = config.monitor_use_power();

            debug!("Updated compressor '{}', monitor_channels: {:?}, process_channels: {:?}, attack: {}, release: {}, threshold: {}, factor: {}, makeup_gain: {}, soft_clip: {}, clip_limit: {:?}, clip_lookahead: {}, clip_use_monitor: {}", self.name, self.process_channels, self.monitor_channels, attack, release, config.threshold, config.factor, config.makeup_gain(), config.soft_clip(), clip_limit, config.clip_lookahead(), config.clip_use_monitor());
        } else {
            // This should never happen unless there is a bug somewhere else
            panic!("Invalid config change!");
        }
    }
}

/// Validate the compressor config, to give a helpful message intead of a panic.
pub fn validate_compressor(config: &config::CompressorParameters) -> Res<()> {
    let channels = config.channels;
    if config.attack <= 0.0 {
        let msg = "Attack value must be larger than zero.";
        return Err(config::ConfigError::new(msg).into());
    }
    if config.release <= 0.0 {
        let msg = "Release value must be larger than zero.";
        return Err(config::ConfigError::new(msg).into());
    }
    for ch in config.monitor_channels().iter() {
        if *ch >= channels {
            let msg = format!(
                "Invalid monitor channel: {}, max is: {}.",
                *ch,
                channels - 1
            );
            return Err(config::ConfigError::new(&msg).into());
        }
    }
    for ch in config.process_channels().iter() {
        if *ch >= channels {
            let msg = format!(
                "Invalid channel to process: {}, max is: {}.",
                *ch,
                channels - 1
            );
            return Err(config::ConfigError::new(&msg).into());
        }
    }
    Ok(())
}

//! Background RF thread: owns the device, publishes spectrum frames, and
//! demodulates whichever channel is selected for audio.

use audio::AudioPlayer;
use common::{Device, GainMode, Hz, Sps, C32};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use dsp::{Deemphasis, FirDecim, FirDecimReal, FmDemod, HighBlend, Mixer, NoiseMeter, Spectrum};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Demod {
    Wfm,
    Nfm,
    Am,
}

impl Demod {
    pub fn label(self) -> &'static str {
        match self {
            Demod::Wfm => "WFM",
            Demod::Nfm => "NFM",
            Demod::Am => "AM",
        }
    }

    /// Occupied channel bandwidth, two-sided.
    pub fn bandwidth(self) -> f64 {
        match self {
            // Carson: 2 * (75 kHz deviation + 15 kHz audio).
            Demod::Wfm => 180_000.0,
            Demod::Nfm => 12_500.0,
            Demod::Am => 10_000.0,
        }
    }

    /// Sample rate to run the demodulator at.
    ///
    /// Comfortably above the channel bandwidth, never equal to it. Decimating
    /// until the output rate matches the bandwidth leaves no transition band,
    /// and the anti-alias filter then needs thousands of taps: 7947 for NFM
    /// against 281 here, with a history buffer too big for L2.
    fn if_rate(self) -> f64 {
        match self {
            Demod::Wfm => 288_000.0,
            Demod::Nfm | Demod::Am => 48_000.0,
        }
    }

    /// Audio bandwidth after demodulation.
    fn audio_bw(self) -> f64 {
        match self {
            Demod::Wfm => 15_000.0,
            Demod::Nfm => 4_000.0,
            Demod::Am => 5_000.0,
        }
    }

    fn deviation(self) -> f64 {
        match self {
            Demod::Wfm => 75_000.0,
            Demod::Nfm => 5_000.0,
            Demod::Am => 0.0,
        }
    }
}

pub enum Cmd {
    Center(Hz),
    Rate(Sps),
    Gain(GainMode),
    /// Tune audio to an offset from centre, or mute.
    Listen(Option<f64>),
    Demod(Demod),
    Volume(f32),
    Fft(usize),
    /// Spectrum frames per second delivered to the UI.
    Refresh(f32),
    /// Exponential averaging applied to the spectrum, 1.0 for none.
    Smoothing(f32),
    Stop,
}

/// One spectrum update.
pub struct Frame {
    pub db: Vec<f32>,
    pub center: f64,
    pub rate: f64,
}

pub struct Status {
    pub dropped: AtomicU64,
    pub running: AtomicBool,
    pub audio_backlog: AtomicU64,
    pub error: parking_lot::Mutex<Option<String>>,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            dropped: AtomicU64::new(0),
            running: AtomicBool::new(false),
            audio_backlog: AtomicU64::new(0),
            error: parking_lot::Mutex::new(None),
        }
    }
}

pub struct Radio {
    pub cmd: Sender<Cmd>,
    pub frames: Receiver<Frame>,
    pub status: Arc<Status>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Radio {
    /// Start streaming from an RTL-SDR. `repaint` is called on every frame so
    /// the UI wakes without polling.
    pub fn start(
        index: u32,
        center: Hz,
        rate: Sps,
        fft: usize,
        repaint: impl Fn() + Send + 'static,
    ) -> Self {
        let (cmd_tx, cmd_rx) = bounded(64);
        // Depth 2: the UI only ever draws the newest spectrum, so queuing more
        // just adds latency between the radio and what is on screen.
        let (frame_tx, frame_rx) = bounded(2);
        let status = Arc::new(Status::default());
        let st = status.clone();

        let handle = std::thread::Builder::new()
            .name("radio".into())
            .spawn(move || {
                if let Err(e) = run(index, center, rate, fft, cmd_rx, frame_tx, &st, repaint) {
                    *st.error.lock() = Some(e.to_string());
                }
                st.running.store(false, Ordering::Relaxed);
            })
            .expect("spawn radio thread");

        Self { cmd: cmd_tx, frames: frame_rx, status, handle: Some(handle) }
    }

    pub fn send(&self, c: Cmd) {
        let _ = self.cmd.try_send(c);
    }
}

impl Drop for Radio {
    fn drop(&mut self) {
        self.send(Cmd::Stop);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Audio chain for one channel, rebuilt whenever the tuning or mode changes.
pub struct Audio {
    mixer: Mixer,
    if_dec: FirDecim,
    demod: FmDemod,
    am: bool,
    au_dec: FirDecimReal,
    deemph: Deemphasis,
    noise: NoiseMeter,
    blend: HighBlend,
    audio_rate: f64,
    shifted: Vec<C32>,
    iq: Vec<C32>,
    disc: Vec<f32>,
    pcm: Vec<f32>,
}

impl Audio {
    pub fn new(offset: f64, rate: f64, mode: Demod, target: f64) -> Self {
        let if_dec = ((rate / mode.if_rate()).round() as usize).max(1);
        let if_rate = rate / if_dec as f64;
        let au_dec = ((if_rate / target).round() as usize).max(1);
        let audio_rate = if_rate / au_dec as f64;

        Self {
            mixer: Mixer::new(-offset, rate),
            if_dec: FirDecim::design_hz(rate, if_dec, mode.bandwidth() / 2.0, 70.0),
            demod: FmDemod::new(if_rate, mode.deviation().max(1.0)),
            am: mode == Demod::Am,
            au_dec: FirDecimReal::design_hz(if_rate, au_dec, mode.audio_bw(), 70.0),
            deemph: Deemphasis::eu(audio_rate),
            noise: NoiseMeter::new(if_rate),
            blend: HighBlend::new(audio_rate),
            audio_rate,
            shifted: Vec::new(),
            iq: Vec::new(),
            disc: Vec::new(),
            pcm: Vec::new(),
        }
    }

    pub fn cost(&self) -> String {
        format!("if {} taps /{}, audio {} taps /{}",
            self.if_dec.taps(), self.if_dec.factor(),
            self.au_dec.taps(), self.au_dec.factor())
    }

    pub fn process(&mut self, input: &[C32], gain: f32) -> &[f32] {
        // Every one of these appends, so they must all be cleared. Missing
        // one makes the buffer grow without bound and each block re-filters
        // the whole accumulated history.
        self.shifted.clear();
        self.mixer.process(input, &mut self.shifted);
        self.iq.clear();
        self.if_dec.process(&self.shifted, &mut self.iq);

        self.disc.clear();
        if self.am {
            self.disc.extend(self.iq.iter().map(|c| c.norm()));
        } else {
            self.demod.process(&self.iq, &mut self.disc);
        }
        let n = self.noise.process(&self.disc);

        self.pcm.clear();
        self.au_dec.process(&self.disc, &mut self.pcm);
        for v in self.pcm.iter_mut() {
            *v *= gain;
        }
        if !self.am {
            self.deemph.process(&mut self.pcm);
        }
        self.blend.process(n, &mut self.pcm);
        &self.pcm
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    index: u32,
    center: Hz,
    rate: Sps,
    fft: usize,
    cmd: Receiver<Cmd>,
    frames: Sender<Frame>,
    status: &Status,
    repaint: impl Fn(),
) -> anyhow::Result<()> {
    let mut dev = rtlsdr::RtlSdr::open(index)?;
    dev.set_rate(rate)?;
    dev.set_center(center)?;
    dev.set_gain("tuner", GainMode::Auto)?;

    let (_player, mut sink) = match AudioPlayer::open(48_000) {
        Ok((p, s)) => (Some(p), Some(s)),
        Err(e) => {
            *status.error.lock() = Some(format!("no audio output: {e}"));
            (None, None)
        }
    };

    let mut spec = Spectrum::new(fft);
    let mut stream = dev.start_rx()?;
    status.running.store(true, Ordering::Relaxed);

    let mut audio: Option<Audio> = None;
    let mut listen: Option<f64> = None;
    let mut mode = Demod::Wfm;
    let mut volume = 0.5f32;
    let mut refresh = 30.0f32;
    let mut next_frame = std::time::Instant::now();
    let mut cur_rate = dev.rate().as_f64();
    let mut cur_center = dev.center().as_f64();
    let mut retune = false;

    loop {
        for c in cmd.try_iter() {
            match c {
                Cmd::Stop => {
                    stream.stop();
                    return Ok(());
                }
                Cmd::Center(f) => {
                    dev.set_center(f)?;
                    cur_center = dev.center().as_f64();
                    spec.reset();
                }
                Cmd::Rate(r) => {
                    dev.set_rate(r)?;
                    cur_rate = dev.rate().as_f64();
                    spec.reset();
                    retune = true;
                }
                Cmd::Gain(g) => dev.set_gain("tuner", g)?,
                Cmd::Listen(o) => {
                    listen = o;
                    retune = true;
                }
                Cmd::Demod(d) => {
                    mode = d;
                    retune = true;
                }
                Cmd::Volume(v) => volume = v,
                Cmd::Fft(n) => {
                    let keep = spec.smoothing;
                    spec = Spectrum::new(n);
                    spec.smoothing = keep;
                }
                Cmd::Refresh(hz) => refresh = hz.clamp(1.0, 120.0),
                Cmd::Smoothing(v) => spec.smoothing = v.clamp(0.01, 1.0),
            }
        }

        if retune {
            audio = listen.map(|off| Audio::new(off, cur_rate, mode, 48_000.0));
            retune = false;
        }

        let read_span = tracing::info_span!("rf_read").entered();
        let buf = match stream.read() {
            Ok(b) => b,
            Err(_) => return Ok(()),
        };
        drop(read_span);
        status.dropped.store(stream.dropped(), Ordering::Relaxed);

        // Only run the FFT and wake the UI when a frame is actually due.
        // Waking on every USB buffer asks for ~260 repaints a second, which
        // pins a core redrawing frames no display can show.
        let now = std::time::Instant::now();
        if now >= next_frame {
            next_frame = now + std::time::Duration::from_secs_f32(1.0 / refresh);
            let _s = tracing::info_span!("spectrum").entered();
            if spec.process(&buf.samples) {
                let f =
                    Frame { db: spec.power_db().to_vec(), center: cur_center, rate: cur_rate };
                // Drop rather than block: the radio must never stall waiting
                // for the UI, and a stale spectrum is worthless anyway.
                match frames.try_send(f) {
                    Ok(()) | Err(TrySendError::Full(_)) => {}
                    Err(TrySendError::Disconnected(_)) => return Ok(()),
                }
                repaint();
            }
        }

        let _a = tracing::info_span!("audio").entered();
        if let (Some(a), Some(s)) = (audio.as_mut(), sink.as_mut()) {
            let rate = a.audio_rate;
            let pcm = a.process(&buf.samples, volume);
            s.write_adaptive(pcm, rate);
            status.audio_backlog.store(s.backlog().max(0) as u64, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(n: usize) -> Vec<C32> {
        (0..n)
            .map(|i| {
                let p = std::f64::consts::TAU * 0.1 * i as f64;
                C32::new(p.cos() as f32 * 0.5, p.sin() as f32 * 0.5)
            })
            .collect()
    }

    #[test]
    fn scratch_buffers_do_not_grow_across_blocks() {
        // Every stage appends to its output. If one is not cleared it grows
        // without bound and each block re-filters the whole history, which
        // looks like the radio slowly seizing up rather than an obvious fault.
        let mut a = Audio::new(120_000.0, 2_304_000.0, Demod::Wfm, 48_000.0);
        let b = block(8192);
        for _ in 0..20 {
            a.process(&b, 0.5);
        }
        assert_eq!(a.shifted.len(), b.len(), "mixer output grew");
        assert!(a.iq.len() <= b.len(), "if output grew");
        assert!(a.disc.len() <= b.len(), "discriminator output grew");
        assert!(a.pcm.len() <= b.len(), "audio output grew");
    }

    #[test]
    fn output_length_is_steady_block_to_block() {
        let mut a = Audio::new(0.0, 2_304_000.0, Demod::Nfm, 48_000.0);
        let b = block(4800);
        let first = a.process(&b, 0.5).len();
        for _ in 0..10 {
            let n = a.process(&b, 0.5).len();
            assert!(
                (n as i64 - first as i64).abs() <= 1,
                "block produced {n} samples after {first}"
            );
        }
    }

    #[test]
    fn every_mode_runs_faster_than_real_time() {
        // The audio chain shares the radio thread with USB draining, so
        // anything near 1x drops samples.
        let rate = 2_304_000.0;
        let b = block(131_072);
        for mode in [Demod::Wfm, Demod::Nfm, Demod::Am] {
            let mut a = Audio::new(120_000.0, rate, mode, 48_000.0);
            a.process(&b, 0.5);
            let t = std::time::Instant::now();
            for _ in 0..4 {
                a.process(&b, 0.5);
            }
            let x = (4.0 * b.len() as f64 / rate) / t.elapsed().as_secs_f64();
            assert!(x > 4.0, "{} only ran at {x:.1}x real time", mode.label());
        }
    }

    #[test]
    fn filters_stay_short_enough_to_sit_in_cache() {
        // Driving the IF rate down to the channel bandwidth leaves no
        // transition band and asks for thousands of taps.
        for mode in [Demod::Wfm, Demod::Nfm, Demod::Am] {
            let a = Audio::new(0.0, 2_304_000.0, mode, 48_000.0);
            assert!(
                a.if_dec.taps() < 600,
                "{} if filter is {} taps",
                mode.label(),
                a.if_dec.taps()
            );
        }
    }

    #[test]
    fn the_audio_rate_is_close_to_what_was_asked_for() {
        for mode in [Demod::Wfm, Demod::Nfm, Demod::Am] {
            let a = Audio::new(0.0, 2_304_000.0, mode, 48_000.0);
            let r = a.audio_rate;
            assert!((r - 48_000.0).abs() < 12_000.0, "{} gave {r} Hz", mode.label());
        }
    }
}

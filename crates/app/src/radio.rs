//! Background RF thread: owns the device, publishes spectrum frames, and
//! demodulates whichever channel is selected for audio.

use audio::AudioPlayer;
use common::{Device, GainMode, Hz, Sps, C32};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use dsp::{Deemphasis, FirDecim, FmDemod, HighBlend, Mixer, NoiseMeter, Spectrum};
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

    /// Channel bandwidth, which sets how far we decimate.
    fn bandwidth(self) -> f64 {
        match self {
            Demod::Wfm => 200_000.0,
            Demod::Nfm => 12_500.0,
            Demod::Am => 10_000.0,
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
struct Audio {
    mixer: Mixer,
    if_dec: FirDecim,
    demod: FmDemod,
    am: bool,
    au_dec: FirDecim,
    deemph: Deemphasis,
    noise: NoiseMeter,
    blend: HighBlend,
    audio_rate: f64,
    shifted: Vec<C32>,
    iq: Vec<C32>,
    disc: Vec<f32>,
    au: Vec<C32>,
    pcm: Vec<f32>,
}

impl Audio {
    fn new(offset: f64, rate: f64, mode: Demod, target: f64) -> Self {
        // Pick integer decimations that land closest to the target audio rate,
        // so the chain needs no rational resampling of its own.
        let if_dec = ((rate / mode.bandwidth()).floor() as usize).max(1);
        let if_rate = rate / if_dec as f64;
        let au_dec = ((if_rate / target).round() as usize).max(1);
        let audio_rate = if_rate / au_dec as f64;

        Self {
            mixer: Mixer::new(-offset, rate),
            if_dec: FirDecim::design(if_dec, 0.8, 70.0),
            demod: FmDemod::new(if_rate, mode.deviation().max(1.0)),
            am: mode == Demod::Am,
            au_dec: FirDecim::design(au_dec, 0.8, 70.0),
            deemph: Deemphasis::eu(audio_rate),
            noise: NoiseMeter::new(if_rate),
            blend: HighBlend::new(audio_rate),
            audio_rate,
            shifted: Vec::new(),
            iq: Vec::new(),
            disc: Vec::new(),
            au: Vec::new(),
            pcm: Vec::new(),
        }
    }

    fn process(&mut self, input: &[C32], gain: f32) -> &[f32] {
        self.mixer.process(input, &mut self.shifted);
        self.iq.clear();
        self.if_dec.process(&self.shifted, &mut self.iq);

        if self.am {
            self.disc.clear();
            self.disc.extend(self.iq.iter().map(|c| c.norm()));
        } else {
            self.demod.process(&self.iq, &mut self.disc);
        }
        let n = self.noise.process(&self.disc);

        // The audio decimator is complex, so carry the real signal through it.
        self.au.clear();
        let real: Vec<C32> = self.disc.iter().map(|&v| C32::new(v, 0.0)).collect();
        self.au_dec.process(&real, &mut self.au);

        self.pcm.clear();
        self.pcm.extend(self.au.iter().map(|c| c.re * gain));
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
            }
        }

        if retune {
            audio = listen.map(|off| Audio::new(off, cur_rate, mode, 48_000.0));
            retune = false;
        }

        let buf = match stream.read() {
            Ok(b) => b,
            Err(_) => return Ok(()),
        };
        status.dropped.store(stream.dropped(), Ordering::Relaxed);

        if spec.process(&buf.samples) {
            let f = Frame { db: spec.power_db().to_vec(), center: cur_center, rate: cur_rate };
            // Drop rather than block: the radio must never stall waiting for
            // the UI, and a stale spectrum is worthless anyway.
            match frames.try_send(f) {
                Ok(()) | Err(TrySendError::Full(_)) => {}
                Err(TrySendError::Disconnected(_)) => return Ok(()),
            }
            repaint();
        }

        if let (Some(a), Some(s)) = (audio.as_mut(), sink.as_mut()) {
            let rate = a.audio_rate;
            let pcm = a.process(&buf.samples, volume);
            s.write_adaptive(pcm, rate);
            status.audio_backlog.store(s.backlog().max(0) as u64, Ordering::Relaxed);
        }
    }
}

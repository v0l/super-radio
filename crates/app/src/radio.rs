//! Background RF thread: owns the device, publishes spectrum frames, and
//! demodulates whichever channel is selected for audio.

use audio::AudioPlayer;
use common::{GainMode, Hz, Sps, C32};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use dsp::{DcBlock, Spectrum};
use nodes::{
    DecimateNode, DeemphasisNode, EnvelopeNode, FmDemodNode, HighBlendNode, MixerNode,
    RealDecimateNode, WfmDemodNode,
};
use pipeline::graph::{Graph, NodeId};
use pipeline::port::StreamSpec;
use std::sync::{
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
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
            // Carson: 2 * (75 kHz deviation + 57 kHz highest modulating
            // frequency). The highest is RDS, not audio: taking 15 kHz gives
            // 180 kHz and cuts off precisely the sidebands that carry the
            // subcarrier, which decodes audio perfectly and RDS barely at all.
            Demod::Wfm => 264_000.0,
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
            // Must clear the 264 kHz occupied bandwidth with room for a
            // transition band.
            Demod::Wfm => 330_000.0,
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

/// Shortest gap between retunes.
///
/// A retune is a blocking USB control transfer costing about 25 ms on the
/// RTL-SDR, and it stalls sample reading while it happens. At this spacing it
/// takes roughly a fifth of the time and the spectrum keeps updating; issuing
/// one per frame instead leaves nothing over to read with and the display
/// freezes for as long as the drag lasts.
const MIN_TUNE_GAP: std::time::Duration = std::time::Duration::from_millis(120);

/// Overridable so the benchmark can measure what happens without the spacing.
fn tune_gap() -> std::time::Duration {
    match std::env::var("SR_TUNE_GAP_MS").ok().and_then(|v| v.parse().ok()) {
        Some(ms) => std::time::Duration::from_millis(ms),
        None => MIN_TUNE_GAP,
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
    /// Remove the centre spur a direct-conversion receiver produces.
    DcBlock(bool),
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
    /// Stereo separation currently applied, as f32 bits.
    blend: AtomicU32,
    /// Station name, programme type and radiotext, when RDS is decoding.
    station: parking_lot::Mutex<StationInfo>,
    /// Shape of the chain currently demodulating, republished on every rebuild.
    chain: parking_lot::Mutex<Option<pipeline::graph::Topology>>,
    /// Delay through that chain in milliseconds, as f32 bits.
    chain_latency: AtomicU32,
}

/// What the UI shows about the tuned station.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StationInfo {
    pub pi: Option<u16>,
    pub name: Option<String>,
    pub pty: Option<&'static str>,
    pub radiotext: Option<String>,
    /// Groups accepted and blocks rejected. The ratio is the honest measure of
    /// RDS reception: a station can be loud and still undecodable.
    pub groups: u64,
    pub block_errors: u64,
    pub synced: bool,
}

impl StationInfo {
    pub fn is_empty(&self) -> bool {
        self.pi.is_none() && self.name.is_none() && self.radiotext.is_none()
    }
}

impl Default for Status {
    fn default() -> Self {
        Self {
            dropped: AtomicU64::new(0),
            running: AtomicBool::new(false),
            audio_backlog: AtomicU64::new(0),
            error: parking_lot::Mutex::new(None),
            blend: AtomicU32::new(0),
            station: parking_lot::Mutex::new(StationInfo::default()),
            chain: parking_lot::Mutex::new(None),
            chain_latency: AtomicU32::new(0),
        }
    }
}

impl Status {
    pub fn blend(&self) -> f32 {
        f32::from_bits(self.blend.load(Ordering::Relaxed))
    }

    fn set_blend(&self, v: f32) {
        self.blend.store(v.to_bits(), Ordering::Relaxed);
    }

    pub fn chain(&self) -> Option<pipeline::graph::Topology> {
        self.chain.lock().clone()
    }

    pub fn chain_latency(&self) -> f64 {
        f64::from(f32::from_bits(self.chain_latency.load(Ordering::Relaxed)))
    }

    fn set_chain(&self, t: Option<pipeline::graph::Topology>, latency_ms: f64) {
        *self.chain.lock() = t;
        self.chain_latency.store((latency_ms as f32).to_bits(), Ordering::Relaxed);
    }

    pub fn station(&self) -> StationInfo {
        self.station.lock().clone()
    }

    fn set_station(&self, s: &dsp::rds::Station, groups: u64, errors: u64, synced: bool) {
        let next = StationInfo {
            pi: s.pi,
            name: s.name.clone(),
            pty: s.pty_name(),
            radiotext: s.radiotext.clone(),
            groups,
            block_errors: errors,
            synced,
        };
        let mut cur = self.station.lock();
        // Only take the lock's write cost when something actually changed;
        // this runs on every audio block.
        if *cur != next {
            *cur = next;
        }
    }

    fn clear_station(&self) {
        *self.station.lock() = StationInfo::default();
        self.set_blend(0.0);
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
        entry: crate::devices::Entry,
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
                if let Err(e) = run(entry, center, rate, fft, cmd_rx, frame_tx, &st, repaint) {
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
///
/// Built as a `pipeline::Graph` from the same nodes the rest of the app uses.
/// The chain was hand-wired before, which meant the graph engine existed and
/// nothing ran on it: rate negotiation, latency reporting and per-node
/// parameters were all reimplemented here or simply absent.
pub struct Audio {
    graph: Graph,
    wfm: Option<NodeId>,
    audio_rate: f64,
    channels: usize,
    iq: Vec<C32>,
    pcm: Vec<f32>,
    detail: String,
    station: dsp::rds::Station,
    stats: (u64, u64, bool),
    blend: f32,
}

impl Audio {
    pub fn new(offset: f64, rate: f64, mode: Demod, target: f64) -> Self {
        let if_dec = ((rate / mode.if_rate()).round() as usize).max(1);
        let if_rate = rate / if_dec as f64;
        let au_dec = ((if_rate / target).round() as usize).max(1);

        let mut b = Graph::builder(StreamSpec::iq(rate, Hz(0)));
        let mix = b.add_labeled("Mixer", Box::new(MixerNode::new(-offset)));
        // Sized from the signal's bandwidth, not from the decimation factor:
        // the stopband has to land where the first alias folds down.
        let mut dec = DecimateNode::new(if_dec);
        dec.set_passband_hz(rate, mode.bandwidth() / 2.0);
        let ifd = b.add_labeled("IF decimator", Box::new(dec));
        b.source(mix.i());
        b.connect(mix.o(), ifd.i());

        let stereo = mode == Demod::Wfm && if_rate >= 130_000.0;
        let mut wfm = None;
        let demod = if stereo {
            let id = b.add_labeled("WFM demod", Box::new(WfmDemodNode::new()));
            wfm = Some(id);
            id
        } else if mode == Demod::Am {
            b.add_labeled("AM envelope", Box::new(EnvelopeNode))
        } else {
            b.add_labeled("FM discriminator", Box::new(FmDemodNode::new(mode.deviation())))
        };
        b.connect(ifd.o(), demod.i());

        let mut ad = RealDecimateNode::new(au_dec);
        ad.set_passband_hz(if_rate, mode.audio_bw());
        let aud = b.add_labeled("Audio decimator", Box::new(ad));
        b.connect(demod.o(), aud.i());
        let last = if mode == Demod::Am {
            aud
        } else {
            let de = b.add_labeled("De-emphasis", Box::new(DeemphasisNode::new(50.0)));
            b.connect(aud.o(), de.i());
            de
        };
        let hb = b.add_labeled("High blend", Box::new(HighBlendNode::new()));
        b.connect(last.o(), hb.i());
        b.output(hb.o());

        let graph = b.build().expect("audio chain");
        let spec = graph.output_spec();
        let detail = format!(
            "if /{if_dec} to {:.0} kHz, audio /{au_dec} to {:.1} kHz{}",
            if_rate / 1e3,
            spec.frame_rate() / 1e3,
            if stereo { ", stereo" } else { "" }
        );
        Self {
            graph,
            wfm,
            audio_rate: spec.frame_rate(),
            channels: spec.channels,
            iq: Vec::new(),
            pcm: Vec::new(),
            detail,
            station: dsp::rds::Station::default(),
            stats: (0, 0, false),
            blend: 0.0,
        }
    }

    pub fn is_stereo(&self) -> bool {
        self.channels == 2
    }

    pub fn stereo_blend(&self) -> f32 {
        self.blend
    }

    pub fn station(&self) -> &dsp::rds::Station {
        &self.station
    }

    /// Groups decoded, blocks rejected, and whether framing is currently held.
    pub fn rds_stats(&self) -> (u64, u64, bool) {
        self.stats
    }

    pub fn cost(&self) -> String {
        self.detail.clone()
    }

    /// Delay through the whole chain, in milliseconds of audio.
    ///
    /// Every filter reports its own group delay and the graph adds them up, so
    /// this is the number to watch rather than any single tap count: a chain
    /// can be built from short filters and still be slow.
    pub fn latency_ms(&self) -> f64 {
        self.graph.output_latency() as f64 / self.audio_rate.max(1.0) * 1e3
    }

    /// The running chain, for the graph view.
    pub fn graph(&self) -> &Graph {
        &self.graph
    }

    pub fn process(&mut self, input: &[C32], gain: f32) -> &[f32] {
        self.iq.clear();
        self.iq.extend_from_slice(input);
        let buf = self.graph.input_buf();
        buf.clear();
        buf.iq_mut().extend_from_slice(&self.iq);
        if self.graph.run().is_err() {
            self.pcm.clear();
            return &self.pcm;
        }
        self.pcm.clear();
        if let Some(out) = self.graph.output().as_real() {
            self.pcm.extend(out.iter().map(|v| v * gain));
        }
        // Read back what the demodulator learned. Events carry the same
        // information but only when it changes, and the status panel wants a
        // current value on every frame rather than the last one it happened to
        // catch.
        if let Some(id) = self.wfm {
            if let Some(n) = self.graph.node(id).and_then(|n| n.as_any()) {
                if let Some(w) = n.downcast_ref::<WfmDemodNode>() {
                    self.station = w.station().clone();
                    self.stats = w.rds_stats();
                    self.blend = w.blend();
                }
            }
        }
        &self.pcm
    }
}

#[allow(clippy::too_many_arguments)]
fn run(
    entry: crate::devices::Entry,
    center: Hz,
    rate: Sps,
    fft: usize,
    cmd: Receiver<Cmd>,
    frames: Sender<Frame>,
    status: &Status,
    repaint: impl Fn(),
) -> anyhow::Result<()> {
    let mut dev = crate::devices::open(&entry)?;
    // Clamp to what this radio can actually do: the app's last span may have
    // come from a different device entirely.
    let info_rates = dev.info().rate_range.clone();
    let rate = Sps(rate.0.clamp(info_rates.start().0, info_rates.end().0));
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
    let mut dc: Option<DcBlock> = None;
    let mut dc_on = true;
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
    let mut want_center: Option<Hz> = None;
    let gap = tune_gap();
    let mut last_tune = std::time::Instant::now() - gap;

    loop {
        for c in cmd.try_iter() {
            match c {
                Cmd::Stop => {
                    stream.stop();
                    return Ok(());
                }
                // Held rather than applied. A drag issues one of these per
                // displayed frame and only the last is worth anything, so
                // applying each in turn spends the whole budget retuning to
                // frequencies already superseded.
                Cmd::Center(f) => want_center = Some(f),
                Cmd::Rate(r) => {
                    dev.set_rate(r)?;
                    cur_rate = dev.rate().as_f64();
                    spec.reset();
                    retune = true;
                    // The notch width is set from the rate.
                    dc = None;
                }
                Cmd::Gain(g) => {
                    dev.set_gain("tuner", g)?;
                    // Gain changes move the offset with it.
                    dc = None;
                }
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
                Cmd::DcBlock(on) => {
                    dc_on = on;
                    dc = None;
                }
            }
        }

        if retune {
            audio = listen.map(|off| Audio::new(off, cur_rate, mode, 48_000.0));
            status.set_chain(
                audio.as_ref().map(|a| a.graph().topology()),
                audio.as_ref().map(|a| a.latency_ms()).unwrap_or(0.0),
            );
            // The old station's name must not linger over the new one.
            status.clear_station();
            retune = false;
        }

        // Retuning costs about 25 ms on the RTL-SDR, more than a frame at
        // 60 Hz, and it blocks the thread that reads samples. Spacing them out
        // keeps the spectrum live while a drag is in progress; the last
        // requested frequency is always reached because the pending one is
        // held until it can be applied.
        if let Some(f) = want_center {
            if last_tune.elapsed() >= gap {
                let _t = tracing::info_span!("set_center").entered();
                dev.set_center(f)?;
                cur_center = dev.center().as_f64();
                spec.reset();
                // The offset differs from tuning to tuning, so re-measure
                // rather than dragging the old one to the new frequency.
                dc = None;
                want_center = None;
                last_tune = std::time::Instant::now();
            }
        }

        let read_span = tracing::info_span!("rf_read").entered();
        let mut buf = match stream.read() {
            Ok(b) => b,
            Err(_) => return Ok(()),
        };
        drop(read_span);
        status.dropped.store(stream.dropped(), Ordering::Relaxed);

        // A direct-conversion front end puts local oscillator leakage and the
        // ADC's offset at exactly the tuned frequency, which reads as a very
        // strong carrier that is not there. Removed before anything else sees
        // it, so the spectrum, the detectors and the audio all agree.
        if dc_on {
            let _d = tracing::info_span!("dc_block").entered();
            let dcb = dc.get_or_insert_with(|| {
                let mut d = DcBlock::new(cur_rate);
                d.prime(&buf.samples);
                d
            });
            dcb.process(&mut buf.samples);
        }

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
            let stereo = a.is_stereo();
            let pcm = a.process(&buf.samples, volume);
            if stereo {
                s.write_adaptive_stereo(pcm, rate);
            } else {
                s.write_adaptive(pcm, rate);
            }
            status.audio_backlog.store(s.backlog().max(0) as u64, Ordering::Relaxed);
            status.set_blend(a.stereo_blend());
            let (g, e, sy) = a.rds_stats();
            status.set_station(a.station(), g, e, sy);
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
        // Measured by time rather than by reaching into buffers, which the
        // chain no longer exposes now it is a graph. A buffer that is never
        // cleared refilters its whole history, so the cost per block climbs;
        // that is the symptom either way.
        let mut a = Audio::new(120_000.0, 2_304_000.0, Demod::Wfm, 48_000.0);
        let b = block(8192);
        let mut cost = |a: &mut Audio| {
            let t = std::time::Instant::now();
            for _ in 0..10 {
                a.process(&b, 0.5);
            }
            t.elapsed().as_secs_f64()
        };
        let first = cost(&mut a);
        for _ in 0..5 {
            cost(&mut a);
        }
        let later = cost(&mut a);
        assert!(
            later < first * 3.0,
            "cost per block climbed from {first:.4}s to {later:.4}s"
        );
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
        //
        // The bound is deliberately far below what any developer machine
        // manages, because it has to hold on the slowest shared CI runner too:
        // this one reads 6.4x here and 3.0x on a two core VM. It is a guard
        // against a chain that has gone accidentally quadratic, not a
        // performance target. Real numbers come from --bench-audio.
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
            assert!(x > 1.5, "{} only ran at {x:.1}x real time", mode.label());
        }
    }

    #[test]
    fn the_chain_does_not_delay_the_audio_audibly() {
        // Driving the IF rate down to the channel bandwidth leaves no
        // transition band and asks for thousands of taps, which shows up as
        // delay. The graph adds up each filter's group delay, so this catches
        // it wherever in the chain it happens rather than at one filter that
        // was remembered to be checked.
        for mode in [Demod::Wfm, Demod::Nfm, Demod::Am] {
            let a = Audio::new(0.0, 2_304_000.0, mode, 48_000.0);
            let ms = a.latency_ms();
            assert!(ms < 40.0, "{} delays audio by {ms:.1} ms", mode.label());
        }
    }

    #[test]
    fn the_chain_is_a_graph_with_every_stage_named() {
        // The point of building on the graph: the stages can be listed, which
        // is what the chain view draws.
        let a = Audio::new(0.0, 2_304_000.0, Demod::Wfm, 48_000.0);
        let names: Vec<&str> = a.graph().order().map(|(_, l)| l).collect();
        assert!(names.contains(&"Mixer"), "{names:?}");
        assert!(names.contains(&"WFM demod"), "{names:?}");
        assert!(names.contains(&"High blend"), "{names:?}");
        assert!(names.iter().all(|n| !n.is_empty()));
    }

    #[test]
    fn am_skips_de_emphasis_and_uses_an_envelope_detector() {
        let a = Audio::new(0.0, 2_304_000.0, Demod::Am, 48_000.0);
        let names: Vec<&str> = a.graph().order().map(|(_, l)| l).collect();
        assert!(names.contains(&"AM envelope"), "{names:?}");
        assert!(!names.contains(&"De-emphasis"), "{names:?}");
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

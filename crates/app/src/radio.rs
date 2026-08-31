//! Background RF thread: owns the device, publishes spectrum frames, and
//! demodulates whichever channel is selected for audio.

use audio::AudioPlayer;
use common::{GainMode, Hz, Sps, C32};
use crossbeam_channel::{bounded, Receiver, Sender, TrySendError};
use dsp::{DcBlock, Spectrum};
use nodes::{
    ChannelBank, DecimateNode, DeemphasisNode, EnvelopeNode, FmDemodNode, Gating, HighBlendNode,
    MixerNode, RealDecimateNode, WfmDemodNode,
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
    /// Decode every channel in the span, or stop doing so.
    Decode(bool),
    /// Write every burst that decodes to this directory, with an optional
    /// budget in megabytes, or stop recording.
    Record(Option<(std::path::PathBuf, Option<u64>)>),
    Stop,
}

/// One spectrum update.
pub struct Frame {
    pub db: Vec<f32>,
    pub center: f64,
    pub rate: f64,
}

/// One decoded packet, as the UI logs and draws it.
#[derive(Clone, Debug, PartialEq)]
pub struct DecodeRecord {
    /// When it was decoded, for ordering the log and placing the waterfall
    /// mark.
    pub at: std::time::Instant,
    /// Centre of the channel it arrived on. Not the tuned frequency: the whole
    /// point is that these come from wherever in the span they happened.
    pub freq: f64,
    /// Width of that channel, which differs between the two banks and is what
    /// says how far apart two reports have to be to be different bursts.
    pub channel_hz: f64,
    /// Protocol name, or "unknown" for a burst nothing claimed.
    pub model: String,
    /// How it was keyed: OOK, FSK, ASK.
    pub modulation: &'static str,
    /// Fields for a decode, inferred coding and timings for an unknown.
    pub detail: String,
    /// The same fields, structured.
    ///
    /// This is what makes the packet list a bus rather than a display: a map,
    /// a chart or an image pane reads these rather than the bytes or the
    /// summary line. See `docs/views.md`.
    pub fields: Vec<(String, common::Value)>,
    /// What the payload is, as a media type, so a view can claim packets it
    /// knows how to render without knowing the protocol that made them.
    pub media_type: &'static str,
    /// Received level in dBFS, and signal to noise in dB.
    pub rssi_dbfs: f32,
    pub snr_db: f32,
    pub bytes: Vec<u8>,
    /// `None` when the protocol has no integrity check, which must stay
    /// visible: an unchecked decode from a noisy band is often wrong.
    pub crc: Option<bool>,
}

impl DecodeRecord {
    /// A bare record, for tests that need one to hand to something else.
    #[cfg(test)]
    pub fn for_test(freq: f64, model: &str) -> Self {
        Self {
            at: std::time::Instant::now(),
            freq,
            channel_hz: 31_250.0,
            model: model.to_string(),
            modulation: "OOK",
            detail: String::new(),
            fields: Vec::new(),
            media_type: pipeline::event::media::BYTES,
            rssi_dbfs: -20.0,
            snr_db: 15.0,
            bytes: vec![1, 2, 3],
            crc: Some(true),
        }
    }

    /// Whether any protocol claimed this burst.
    pub fn is_known(&self) -> bool {
        self.model != "unknown"
    }
}

/// Decodes everything audible in the current span, without being tuned or told
/// what to look for.
///
/// Two channelizers, not one, because the two front ends want opposite things
/// from a channel. Measured on the Fine Offset capture by adding noise until
/// decoding stops, a 1.5 kbit/s OOK sensor survives down to 12.3 dB
/// peak-to-noise in a 31 kHz channel and only 22.9 dB in a 125 kHz one: the
/// detector integrates noise across the whole channel while the signal
/// occupies a sliver of it, so a wide channel costs 10.6 dB for nothing. An
/// FSK transmitter needs the opposite, because its two tones are tens of kHz
/// apart and a narrow channel simply cuts one of them off: the same synthetic
/// packet reads as 46 bits at 110 us a symbol in a 125 kHz channel and as
/// eight bits of nonsense in a 31 kHz one.
///
/// Neither front end needs the signal centred, which is what makes any of this
/// work: the OOK path is an envelope detector and does not care where in the
/// channel the carrier sits, and the FSK path measures both tones from the
/// burst itself, so a SAW transmitter tens of kHz off nominal reads the same
/// as one on frequency. Width therefore costs sensitivity and nothing else.
struct Scanner {
    /// Narrow channels running the OOK front end.
    narrow: ChannelBank,
    /// Wide channels running the FSK front end.
    wide: ChannelBank,
    rate: f64,
    hits: u64,
    /// Bursts already reported, for long enough to recognise the same one
    /// arriving again from another channel.
    recent: Vec<Reported>,
}

/// A burst that has already been logged.
#[derive(Clone, Copy, Debug)]
struct Reported {
    at: std::time::Instant,
    freq: f64,
    channel_hz: f64,
    modulation: &'static str,
}

/// How long a burst stays in that memory.
///
/// A block is roughly a tenth of a second, and a burst that starts near the
/// end of one is finished by the detectors in the next, so its copies from
/// neighbouring channels straddle the boundary and a per-block comparison
/// misses half of them. Measured on live 868 MHz traffic, one transmission
/// appeared as four rows 31 kHz apart across two blocks.
///
/// Long enough to cover that, short enough that a device repeating its packet
/// two or three times a second still gets a row per repeat.
const DEDUPE_WINDOW: std::time::Duration = std::time::Duration::from_millis(300);

/// Channel width for the OOK bank. Below this the measurements show no further
/// gain, because the sensor's own bandwidth and its carrier offset start to
/// matter more than the noise saved.
const OOK_CHANNEL_HZ: f64 = 31_250.0;
/// Channel width for the FSK bank. rtl_433 runs at 250 kHz for the same
/// signals; half that still holds a 50 kHz tone separation comfortably.
const FSK_CHANNEL_HZ: f64 = 125_000.0;

impl Scanner {
    /// Channels a span splits into at a given width.
    fn channels_for(rate: f64, width_hz: f64) -> usize {
        let n = (rate / width_hz).round() as usize;
        // The channelizer requires an even count, and a single channel would
        // be a decimator with extra steps.
        (n.clamp(2, 1024) + 1) & !1
    }

    fn bank(rate: f64, center: Hz, width_hz: f64) -> ChannelBank {
        let channels = Self::channels_for(rate, width_hz);
        // 12 taps per branch is about 90 dB of channel-to-channel isolation,
        // enough that a strong transmitter does not paint copies of itself
        // across the band and decode several times over.
        let mut b = ChannelBank::new(channels, 12, rate, center);
        b.set_gating(Gating::OnDetection);
        b.set_detector_config(nodes::ism_detector_config());
        b
    }

    fn new(rate: f64, center: Hz) -> Self {
        let mut narrow = Self::bank(rate, center, OOK_CHANNEL_HZ);
        let mut wide = Self::bank(rate, center, FSK_CHANNEL_HZ);
        // Building every graph can only fail if the chain itself is malformed,
        // which is a bug rather than a runtime condition.
        narrow.set_all_graphs(nodes::ism_ook_graph).expect("OOK decode graph");
        wide.set_all_graphs(nodes::ism_fsk_graph).expect("FSK decode graph");
        Self { narrow, wide, rate, hits: 0, recent: Vec::new() }
    }

    /// Whether a burst is new, remembering it if so.
    ///
    /// Deduping within a block is not enough. Reads from the radio are short,
    /// about seven milliseconds at 2.3 MS/s, and a burst that starts near the
    /// end of one is finished by the detectors in the next, so the copies from
    /// neighbouring channels straddle the boundary. Measured on live 868 MHz
    /// traffic, one transmission appeared as four rows 31 kHz apart.
    fn accept(&mut self, r: &DecodeRecord, now: std::time::Instant) -> bool {
        self.recent.retain(|k| now.saturating_duration_since(k.at) < DEDUPE_WINDOW);
        if self.recent.iter().any(|k| same_burst(k, r)) {
            return false;
        }
        self.recent.push(Reported {
            at: r.at,
            freq: r.freq,
            channel_hz: r.channel_hz,
            modulation: r.modulation,
        });
        true
    }

    /// Channels in each bank, narrow first.
    fn channels(&self) -> (usize, usize) {
        (self.narrow.channels(), self.wide.channels())
    }

    fn retune(&mut self, center: Hz) {
        self.narrow.set_center(center);
        self.wide.set_center(center);
        // Every channel covers a different frequency now, so nothing already
        // reported can be the same burst as anything arriving next.
        self.recent.clear();
    }

    /// Run a block and append whatever decoded.
    fn process(&mut self, iq: &[C32], out: &mut Vec<DecodeRecord>) {
        // Stamped at the start of the block rather than at the moment the
        // decode fell out of it. The packet happened somewhere inside the
        // block, and a pulse detector only closes a package once it has seen
        // the silence afterwards, so "now" is always late by up to a block.
        // Anything drawing this against a waterfall would put the mark below
        // the trace it belongs to.
        let block = std::time::Duration::from_secs_f64(iq.len() as f64 / self.rate.max(1.0));
        let now = std::time::Instant::now() - block;
        let first = out.len();

        let mut found = Vec::new();
        for (bank, width) in [
            (&mut self.narrow, OOK_CHANNEL_HZ),
            (&mut self.wide, FSK_CHANNEL_HZ),
        ] {
            let width = width.min(bank.channel_bandwidth());
            let Ok(evs) = bank.process(iq) else { continue };
            for ev in evs {
                // Warnings are per burst and per channel, so across a whole
                // band they arrive in the thousands. The log is for packets.
                if let pipeline::event::Event::Decoded(d) = &ev.event {
                    found.push(DecodeRecord {
                        at: now,
                        freq: ev.center.as_f64(),
                        channel_hz: width,
                        model: d.protocol.to_string(),
                        modulation: d.modulation.unwrap_or("?"),
                        detail: d.detail.clone().or_else(|| d.text.clone()).unwrap_or_default(),
                        fields: d.fields.clone(),
                        media_type: d.media_type,
                        rssi_dbfs: d.rssi_dbfs.unwrap_or(f32::NAN),
                        snr_db: d.snr_db.unwrap_or(f32::NAN),
                        bytes: d.payload.clone(),
                        crc: d.crc_ok,
                    });
                }
            }
        }

        dedupe_neighbours(&mut found);
        for r in found.into_iter().filter(|r| !r.model.is_empty()) {
            if self.accept(&r, now) {
                out.push(r);
            }
        }
        self.hits += (out.len() - first) as u64;
    }
}

/// Scan a buffer while recording, as the radio thread does. Test support.
#[cfg(test)]
pub fn scan_with_recorder(
    buf: &common::IqBuf,
    rec: &mut crate::record::Recorder,
) -> Vec<DecodeRecord> {
    let mut sc = Scanner::new(buf.rate.as_f64(), buf.center);
    let mut out = Vec::new();
    for block in buf.samples.chunks(16_384) {
        let seen = out.len();
        rec.push(block);
        sc.process(block, &mut out);
        for d in &out[seen..] {
            rec.capture(d);
        }
    }
    out
}

/// Run a capture through the same scanner the live receiver uses.
///
/// The point of recording bursts is to be able to try again without waiting
/// for a device to transmit, so replay has to go through the same code the
/// receiver does, not a simplified copy of it. Blocks are the size the radio
/// delivers, because the scanner's deduplication depends on how a burst falls
/// across block boundaries and a whole-file call would not exercise it.
pub fn replay(path: impl AsRef<std::path::Path>) -> anyhow::Result<Vec<DecodeRecord>> {
    let src = sources::FileSource::open(path.as_ref())?;
    let buf = src.read_all()?;
    let mut sc = Scanner::new(buf.rate.as_f64(), buf.center);
    let mut out = Vec::new();
    for block in buf.samples.chunks(16_384) {
        sc.process(block, &mut out);
    }
    Ok(out)
}

/// Drop the copies of a burst that other channels also reported.
///
/// Channels overlap by design: a two times oversampled channelizer hands
/// adjacent channels each other's transition band, so a transmitter sitting
/// anywhere near an edge is genuinely present in two of them, and its
/// sidebands reach further still. Each of those channels runs its own
/// detector, reads a mangled copy of the same burst, and reports it. Measured
/// on a synthetic FSK packet, the channel holding the signal read it correctly
/// as 46 bits at 110 us a symbol while its neighbour reported 139 bits at
/// 36 us: not a second device, just the same one seen through a filter skirt.
/// Running two banks over the same air makes this certain rather than likely.
///
/// The strongest report of a burst wins, and a real decode beats an unknown
/// however loud, because a protocol that matched its own CRC is better
/// evidence than a stronger guess. Marked by clearing the model rather than
/// removed here, so the caller can compact once.
/// Whether a new report is the same burst as one already logged.
///
/// Same channel through the same front end is a second transmission, which on
/// a device that repeats its packet is exactly what should be logged. Same
/// channel through the other front end is one burst read twice. Anything else
/// near enough in frequency is one signal seen through a filter skirt: two and
/// a half channels either side, taken from the wider of the two reports
/// because that is the one whose skirts reach furthest.
fn same_burst(kept: &Reported, new: &DecodeRecord) -> bool {
    let d = (kept.freq - new.freq).abs();
    if d < 1.0 && (kept.channel_hz - new.channel_hz).abs() < 1.0 {
        return kept.modulation != new.modulation;
    }
    d <= 2.5 * kept.channel_hz.max(new.channel_hz)
}

fn dedupe_neighbours(block: &mut [DecodeRecord]) {
    let mut order: Vec<usize> = (0..block.len()).collect();
    order.sort_by(|&a, &b| {
        let key = |r: &DecodeRecord| (r.is_known(), r.rssi_dbfs);
        let (ka, kb) = (key(&block[a]), key(&block[b]));
        kb.0.cmp(&ka.0).then(kb.1.total_cmp(&ka.1))
    });

    let mut kept: Vec<(f64, f64, &'static str)> = Vec::new();
    for i in order {
        let dup = kept.iter().any(|(kf, kw, km)| {
            same_burst(
                &Reported {
                    at: block[i].at,
                    freq: *kf,
                    channel_hz: *kw,
                    modulation: km,
                },
                &block[i],
            )
        });
        if dup {
            block[i].model.clear();
        } else {
            kept.push((block[i].freq, block[i].channel_hz, block[i].modulation));
        }
    }
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
    /// Packets decoded across the whole span since the radio started.
    pub decoded: AtomicU64,
    /// Channels each bank is splitting the span into, zero when decoding is
    /// off. Narrow ones run the OOK front end, wide ones the FSK front end.
    pub scan_channels: AtomicU64,
    pub scan_channels_wide: AtomicU64,
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
            decoded: AtomicU64::new(0),
            scan_channels: AtomicU64::new(0),
            scan_channels_wide: AtomicU64::new(0),
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
    /// Packets decoded anywhere in the span, in the order they were found.
    pub decodes: Receiver<Vec<DecodeRecord>>,
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
        // Deeper than the spectrum queue, and for the opposite reason: a
        // dropped spectrum frame is replaced 30 times a second, but a dropped
        // decode is a packet that will not come again.
        let (dec_tx, dec_rx) = bounded(64);
        let status = Arc::new(Status::default());
        let st = status.clone();

        let handle = std::thread::Builder::new()
            .name("radio".into())
            .spawn(move || {
                if let Err(e) =
                    run(entry, center, rate, fft, cmd_rx, frame_tx, dec_tx, &st, repaint)
                {
                    *st.error.lock() = Some(e.to_string());
                }
                st.running.store(false, Ordering::Relaxed);
            })
            .expect("spawn radio thread");

        Self { cmd: cmd_tx, frames: frame_rx, decodes: dec_rx, status, handle: Some(handle) }
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
    decodes: Sender<Vec<DecodeRecord>>,
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
    // On from the start: a receiver that only decodes what you tuned to will
    // miss the sensor that transmitted once while you were reading the
    // spectrum, and that transmission is the whole reason to be here.
    let mut scan: Option<Scanner> = Some(Scanner::new(dev.rate().as_f64(), dev.center()));
    let mut scan_on = true;
    let mut rec: Option<crate::record::Recorder> = None;
    let mut records: Vec<DecodeRecord> = Vec::new();
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
                    // A different span is a different set of channels, so the
                    // bank is rebuilt rather than retuned.
                    scan = scan_on.then(|| Scanner::new(cur_rate, dev.center()));
                    if let Some(r) = rec.as_mut() {
                        r.retune(cur_rate, dev.center());
                    }
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
                Cmd::Record(dir) => {
                    rec = match dir {
                        Some((d, mb)) => match crate::record::Recorder::new(&d, cur_rate, Hz(cur_center as u64)) {
                            Ok(r) => Some(match mb {
                                Some(mb) => r.with_budget(mb << 20),
                                None => r,
                            }),
                            Err(e) => {
                                *status.error.lock() = Some(format!("cannot record to {}: {e}", d.display()));
                                None
                            }
                        },
                        None => None,
                    };
                }
                Cmd::Decode(on) => {
                    scan_on = on;
                    scan = on.then(|| Scanner::new(cur_rate, dev.center()));
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
                if let Some(s) = scan.as_mut() {
                    s.retune(dev.center());
                }
                if let Some(r) = rec.as_mut() {
                    r.retune(cur_rate, dev.center());
                }
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

        {
            let _s = tracing::info_span!("scan").entered();
            if let Some(sc) = scan.as_mut() {
                records.clear();
                // The recorder sees the block before the scanner does, so a
                // burst the scanner then reports is already in its history.
                if let Some(r) = rec.as_mut() {
                    r.push(&buf.samples);
                }
                sc.process(&buf.samples, &mut records);
                if let Some(r) = rec.as_mut() {
                    for d in &records {
                        r.capture(d);
                    }
                    if r.is_full() {
                        *status.error.lock() = Some(format!(
                            "recording stopped: wrote {} MB",
                            r.written() >> 20
                        ));
                        rec = None;
                    }
                }
                let (narrow, wide) = sc.channels();
                status.scan_channels.store(narrow as u64, Ordering::Relaxed);
                status.scan_channels_wide.store(wide as u64, Ordering::Relaxed);
                if !records.is_empty() {
                    status.decoded.store(sc.hits, Ordering::Relaxed);
                    // Never block the radio thread on a UI that is behind; a
                    // dropped batch is reported by the counter going up
                    // without the log growing to match.
                    match decodes.try_send(std::mem::take(&mut records)) {
                        Ok(()) | Err(TrySendError::Full(_)) => {}
                        Err(TrySendError::Disconnected(_)) => return Ok(()),
                    }
                    records = Vec::new();
                    repaint();
                }
            } else {
                status.scan_channels.store(0, Ordering::Relaxed);
                status.scan_channels_wide.store(0, Ordering::Relaxed);
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


    fn fixture() -> Option<common::IqBuf> {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/fineoffset_wh1080_433.92M_250k.cu8");
        if !p.exists() {
            return None;
        }
        sources::FileSource::open(&p).ok()?.read_all().ok()
    }

    #[test]
    fn each_bank_splits_the_span_to_the_width_its_front_end_wants() {
        for rate in [250_000.0, 1_024_000.0, 2_400_000.0, 20_000_000.0] {
            for (want, lo, hi) in [
                (OOK_CHANNEL_HZ, 15_000.0, 70_000.0),
                (FSK_CHANNEL_HZ, 60_000.0, 260_000.0),
            ] {
                let n = Scanner::channels_for(rate, want);
                assert_eq!(n % 2, 0, "{rate} at {want} Hz gave an odd count {n}");
                let width = rate / n as f64;
                assert!(
                    (lo..hi).contains(&width) || n == 2,
                    "{rate} at {want} Hz gave {n} channels of {width} Hz"
                );
            }
        }
    }

    #[test]
    fn the_ook_bank_is_the_narrower_of_the_two() {
        // The whole reason for two banks. Measured on the Fine Offset capture,
        // a 31 kHz channel decodes it down to 12.3 dB peak-to-noise where a
        // 125 kHz channel needs 22.9 dB.
        let sc = Scanner::new(2_400_000.0, Hz::mhz(868));
        let (narrow, wide) = sc.channels();
        assert!(narrow > wide, "{narrow} narrow against {wide} wide");
        assert!(sc.narrow.channel_bandwidth() < sc.wide.channel_bandwidth());
    }

    #[test]
    fn a_narrow_span_still_gets_a_usable_bank() {
        // Two channels is the floor: the channelizer needs an even count and
        // one channel would just be a decimator.
        assert_eq!(Scanner::channels_for(1_000.0, OOK_CHANNEL_HZ), 2);
        assert!(
            Scanner::channels_for(1e9, OOK_CHANNEL_HZ) <= 1024,
            "the count has to stay bounded"
        );
    }

    #[test]
    fn the_scanner_decodes_a_real_transmission_without_being_tuned_to_it() {
        // Nothing here selects a frequency, a modulation or a protocol. The
        // capture is fed in as if it had just arrived from the device.
        let Some(buf) = fixture() else {
            eprintln!("skipping: fixture absent, run testdata/fetch.sh");
            return;
        };
        let mut sc = Scanner::new(buf.rate.as_f64(), buf.center);
        let mut out = Vec::new();
        for block in buf.samples.chunks(65_536) {
            sc.process(block, &mut out);
        }

        assert!(!out.is_empty(), "nothing decoded from a capture that contains a packet");
        // Unrecognised bursts are reported too, so pick out the real one
        // rather than assuming it arrived first.
        let r = out
            .iter()
            .find(|r| r.model == "Fineoffset-WHx080")
            .unwrap_or_else(|| panic!("only unknowns: {out:?}"));
        assert_eq!(r.crc, Some(true), "{r:?}");
        assert!(r.detail.contains("temperature_c=16.2"), "{}", r.detail);
        // Structured, not just printed: a chart or a map has to be able to
        // read a field without parsing the summary line back apart.
        assert_eq!(
            r.fields.iter().find(|(k, _)| k == "temperature_c").map(|(_, v)| v.as_f64()),
            Some(Some(16.2))
        );
        assert_eq!(r.modulation, "OOK");
        // A real reception from a recording made near full scale: strong, and
        // well clear of the noise.
        assert!(r.snr_db > 6.0, "snr came out as {}", r.snr_db);
        // Referenced to full scale at the detector, so filter gain can put a
        // very strong packet slightly over zero. What matters is that it is a
        // real measurement rather than a placeholder.
        assert!(
            (-60.0..=6.0).contains(&r.rssi_dbfs),
            "rssi came out as {} dB",
            r.rssi_dbfs
        );
        // One row, not five: the FSK branch reads the same burst and the
        // neighbouring channels see its skirts, and all of that is one packet.
        assert_eq!(out.len(), 1, "the same burst was logged more than once: {out:#?}");
        // The frequency reported is the channel's, not the tuner's, which is
        // what makes a waterfall mark land on the signal.
        let off = (r.freq - buf.center.as_f64()).abs();
        assert!(off < buf.rate.as_f64() / 2.0, "{} Hz is outside the span", r.freq);
    }

    fn rec(freq: f64, model: &str, rssi: f32) -> DecodeRecord {
        DecodeRecord {
            at: std::time::Instant::now(),
            freq,
            model: model.into(),
            channel_hz: 125_000.0,
            modulation: "FSK",
            detail: String::new(),
            fields: Vec::new(),
            media_type: pipeline::event::media::BYTES,
            rssi_dbfs: rssi,
            snr_db: 20.0,
            bytes: vec![1, 2, 3],
            crc: None,
        }
    }

    #[test]
    fn the_same_burst_seen_by_two_channels_is_reported_once() {
        // Channels overlap, so a strong transmitter is genuinely present in
        // its neighbours, where the detectors read a mangled copy of it. The
        // loudest reading wins and the skirts are dropped.
        let w = 125_000.0;
        let mut block = vec![
            rec(868_100_000.0, "unknown", -54.0),
            rec(868_100_000.0 + w, "unknown", -38.0),
            rec(868_100_000.0 - w, "unknown", -61.0),
        ];
        assert_eq!(block[0].modulation, "FSK");
        dedupe_neighbours(&mut block);
        let kept: Vec<&DecodeRecord> = block.iter().filter(|r| !r.model.is_empty()).collect();
        assert_eq!(kept.len(), 1, "kept {kept:#?}");
        assert_eq!(kept[0].rssi_dbfs, -38.0, "the strongest reading should win");
    }

    #[test]
    fn a_real_decode_beats_a_louder_guess() {
        let mut block = vec![
            rec(868_100_000.0, "unknown", -20.0),
            rec(868_100_000.0 + 125_000.0, "Fineoffset-WHx080", -44.0),
        ];
        dedupe_neighbours(&mut block);
        let kept: Vec<&DecodeRecord> = block.iter().filter(|r| !r.model.is_empty()).collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].model, "Fineoffset-WHx080", "a CRC beats a stronger guess");
    }

    #[test]
    fn two_devices_far_apart_are_both_kept() {
        let mut block =
            vec![rec(868_100_000.0, "unknown", -40.0), rec(869_000_000.0, "unknown", -50.0)];
        dedupe_neighbours(&mut block);
        assert_eq!(block.iter().filter(|r| !r.model.is_empty()).count(), 2);
    }

    #[test]
    fn a_device_that_repeats_its_packet_is_logged_every_time() {
        // Two bursts on one channel through one front end are two
        // transmissions, not one seen twice, and a sensor that sends its
        // reading three times should show three rows.
        let mut block = vec![
            rec(868_100_000.0, "unknown", -40.0),
            rec(868_100_000.0, "unknown", -41.0),
        ];
        dedupe_neighbours(&mut block);
        assert_eq!(block.iter().filter(|r| !r.model.is_empty()).count(), 2);
    }

    #[test]
    fn one_burst_read_by_both_front_ends_is_logged_once() {
        // The OOK and FSK branches see the same channel, so a burst can be
        // decoded by one and guessed at by the other. That is one packet.
        let mut ook = rec(868_100_000.0, "Fineoffset-WHx080", -44.0);
        ook.modulation = "OOK";
        let mut block = vec![rec(868_100_000.0, "unknown", -30.0), ook];
        dedupe_neighbours(&mut block);
        let kept: Vec<&DecodeRecord> = block.iter().filter(|r| !r.model.is_empty()).collect();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].model, "Fineoffset-WHx080");
    }

    fn ook_at(freq: f64, at: std::time::Instant) -> DecodeRecord {
        let mut r = rec(freq, "unknown", -30.0);
        r.channel_hz = OOK_CHANNEL_HZ;
        r.modulation = "OOK";
        r.at = at;
        r
    }

    #[test]
    fn a_burst_split_across_two_blocks_is_still_reported_once() {
        // Observed on live 868 MHz traffic: one transmission arrived as four
        // rows 31 kHz apart, because reads from the radio are milliseconds
        // long and each was deduped alone.
        let mut sc = Scanner::new(2_400_000.0, Hz::mhz(868));
        let t0 = std::time::Instant::now();
        let block = std::time::Duration::from_millis(7);
        let mut kept = 0;
        for (n, freq) in [868_362_300.0, 868_393_400.0, 868_331_100.0].iter().enumerate() {
            let at = t0 + block * n as u32;
            if sc.accept(&ook_at(*freq, at), at) {
                kept += 1;
            }
        }
        assert_eq!(kept, 1, "one burst logged as {kept} rows");
    }

    #[test]
    fn a_device_repeating_on_its_own_channel_is_logged_every_time() {
        // Same channel through the same front end is a second transmission,
        // not a second reading of the first, and a sensor that sends its
        // packet three times should show three rows.
        let mut sc = Scanner::new(2_400_000.0, Hz::mhz(868));
        let t0 = std::time::Instant::now();
        for n in 0..3u32 {
            let at = t0 + std::time::Duration::from_millis(60) * n;
            assert!(sc.accept(&ook_at(868_362_300.0, at), at), "repeat {n} was swallowed");
        }
    }

    #[test]
    fn a_neighbour_is_only_a_duplicate_while_the_burst_is_recent() {
        let mut sc = Scanner::new(2_400_000.0, Hz::mhz(868));
        let t0 = std::time::Instant::now();
        assert!(sc.accept(&ook_at(868_362_300.0, t0), t0));

        let soon = t0 + std::time::Duration::from_millis(50);
        assert!(!sc.accept(&ook_at(868_393_400.0, soon), soon), "a skirt slipped through");

        // Long enough later and it is a different burst that happens to be
        // next door, which is the whole reason the memory expires.
        let later = t0 + DEDUPE_WINDOW + std::time::Duration::from_millis(10);
        assert!(sc.accept(&ook_at(868_393_400.0, later), later), "the memory never expired");
    }

    #[test]
    fn the_dedupe_memory_is_shorter_than_a_repeating_device() {
        // Long enough to cover a block boundary, short enough that a sensor
        // repeating its packet two or three times a second still gets a row
        // per repeat.
        assert!(DEDUPE_WINDOW >= std::time::Duration::from_millis(250));
        assert!(DEDUPE_WINDOW <= std::time::Duration::from_millis(400));
    }

    #[test]
    fn a_decode_is_stamped_when_the_block_started_not_when_it_finished() {
        let Some(buf) = fixture() else {
            eprintln!("skipping: fixture absent, run testdata/fetch.sh");
            return;
        };
        let mut sc = Scanner::new(buf.rate.as_f64(), buf.center);
        let mut out = Vec::new();
        let t0 = std::time::Instant::now();
        for block in buf.samples.chunks(65_536) {
            sc.process(block, &mut out);
        }
        let rec = out.first().expect("a decode");
        // 65536 samples at 250 kS/s is 262 ms of signal, and the packet is
        // somewhere inside that, so the stamp must precede the call that
        // produced it by about a block.
        let back = t0.duration_since(rec.at).as_secs_f64();
        assert!(back > 0.0, "stamped {back}s after the block it came from");
    }

    #[test]
    fn retuning_clears_state_rather_than_carrying_it_across() {
        // A burst half-collected at one frequency must not finish at another.
        let mut sc = Scanner::new(250_000.0, Hz::mhz(433));
        let mut out = Vec::new();
        sc.process(&block(8192), &mut out);
        sc.retune(Hz::mhz(868));
        sc.process(&block(8192), &mut out);
        assert!(out.is_empty(), "a steady tone decoded as {out:?}");
    }

    #[test]
    fn the_scanner_keeps_up_with_the_stream() {
        // Decoding the whole span is only worth having if it runs in real
        // time; if it does not, it is stealing from the thread that has to
        // drain USB and the radio drops samples instead.
        if cfg!(debug_assertions) {
            eprintln!("skipping: an unoptimised build says nothing about throughput");
            return;
        }
        let rate = 2_400_000.0;
        let mut sc = Scanner::new(rate, Hz::mhz(868));
        let b = block(262_144);
        let mut out = Vec::new();
        // One pass to warm the filters and the pool.
        sc.process(&b, &mut out);

        let t = std::time::Instant::now();
        let blocks = 20;
        for _ in 0..blocks {
            out.clear();
            sc.process(&b, &mut out);
        }
        let secs = t.elapsed().as_secs_f64();
        let audio_secs = blocks as f64 * b.len() as f64 / rate;
        let x = audio_secs / secs;
        let (narrow, wide) = sc.channels();
        eprintln!("scanner: {x:.1}x real time on {narrow} narrow + {wide} wide channels");
        assert!(
            x > 1.0,
            "the scanner ran at only {x:.2}x real time ({narrow} narrow + {wide} wide)"
        );
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

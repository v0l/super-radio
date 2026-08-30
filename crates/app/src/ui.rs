//! Instrument front panel: readout, spectrum, waterfall, channel strips.

use crate::bands;
use crate::dial::Dial;
use crate::radio::{Cmd, Demod, Frame, Radio};
use crate::theme::{self, legend, readout, value};
use crate::waterfall::Waterfall;
use crate::wheel::Wheel;
use common::{GainMode, Hz, Sps};
use egui::containers::{CentralPanel, Panel};
use egui::{Align2, Color32, FontFamily, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};

pub struct App {
    radio: Option<Radio>,
    err: Option<String>,

    center: f64,
    rate: f64,
    gain: GainMode,

    db: Vec<f32>,
    wf: Waterfall,
    floor: f32,
    ceil: f32,
    auto_scale: bool,

    dial: Dial,
    /// Centre the waterfall history currently corresponds to, so a retune can
    /// slide it instead of throwing it away.
    wf_center: f64,
    wf_pending: Vec<f32>,
    wf_last: Option<std::time::Instant>,
    rows_per_sec: f32,
    refresh: f32,
    fft_size: usize,
    scrub: Wheel,
    channels: Vec<Channel>,
    listening: Option<usize>,
    volume: f32,
    next_id: u32,
    fft: usize,
    /// Run for this many seconds, report CPU used, then quit.
    pub soak: Option<f32>,
    /// Save a PNG to this path once the radio has settled, then quit.
    pub shot: Option<String>,
    shot_at: Option<std::time::Instant>,
    shot_sent: bool,
}

pub struct Channel {
    freq: f64,
    demod: Demod,
    label: String,
}

const FFTS: [usize; 6] = [512, 1024, 2048, 4096, 8192, 16384];
/// Spectrum refresh rates in frames per second.
const REFRESH: [(&str, f32); 4] = [("10", 10.0), ("20", 20.0), ("30", 30.0), ("60", 60.0)];
/// Waterfall scroll rates in rows per second.
const SPEEDS: [(&str, f32); 5] = [
    ("5", 5.0),
    ("10", 10.0),
    ("20", 20.0),
    ("40", 40.0),
    ("80", 80.0),
];

const SPANS: [(&str, f64); 5] = [
    ("250k", 250_000.0),
    ("1.024M", 1_024_000.0),
    ("2.048M", 2_048_000.0),
    ("2.304M", 2_304_000.0),
    ("2.4M", 2_400_000.0),
];

impl Default for App {
    fn default() -> Self {
        Self {
            radio: None,
            err: None,
            center: 95_800_000.0,
            rate: 2_304_000.0,
            gain: GainMode::Auto,
            db: Vec::new(),
            wf: Waterfall::new(512),
            floor: -90.0,
            ceil: -20.0,
            auto_scale: true,
            dial: Dial::new(),
            wf_center: 95_800_000.0,
            wf_pending: Vec::new(),
            wf_last: None,
            rows_per_sec: 20.0,
            refresh: 30.0,
            fft_size: 2048,
            scrub: Wheel::default(),
            channels: Vec::new(),
            listening: None,
            volume: 0.5,
            next_id: 1,
            fft: 2048,
            soak: None,
            shot: None,
            shot_at: None,
            shot_sent: false,
        }
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        theme::install(&cc.egui_ctx);
        let mut app = Self::default();
        app.connect(&cc.egui_ctx);
        app
    }

    fn connect(&mut self, ctx: &egui::Context) {
        let c = ctx.clone();
        self.radio = Some(Radio::start(
            0,
            Hz(self.center as u64),
            Sps(self.rate as u64),
            self.fft,
            move || c.request_repaint(),
        ));
        self.wf.clear();
    }

    fn send(&self, c: Cmd) {
        if let Some(r) = &self.radio {
            r.send(c);
        }
    }

    fn drain(&mut self) {
        let Some(radio) = &self.radio else { return };
        if let Some(e) = radio.status.error.lock().take() {
            self.err = Some(e);
        }
        let mut latest: Option<Frame> = None;
        while let Ok(f) = radio.frames.try_recv() {
            latest = Some(f);
        }
        if let Some(f) = latest {
            self.center = f.center;
            self.rate = f.rate;
            if self.auto_scale {
                self.rescale(&f.db);
            }
            self.slide_waterfall(f.db.len());

            // Hold the peak between rows rather than sampling one frame in N,
            // or a short burst lands between rows and is never drawn.
            if self.wf_pending.len() != f.db.len() {
                self.wf_pending = f.db.clone();
            } else {
                for (a, b) in self.wf_pending.iter_mut().zip(&f.db) {
                    *a = a.max(*b);
                }
            }
            let due = self
                .wf_last
                .map(|t| t.elapsed().as_secs_f32() >= 1.0 / self.rows_per_sec)
                .unwrap_or(true);
            if due {
                // The waterfall tops out below the trace's ceiling: the plot
                // wants headroom so peaks are not clipped flat, the colour
                // ramp wants the opposite or its hottest colours go unused.
                let pending = std::mem::take(&mut self.wf_pending);
                self.wf.push(&pending, self.floor, self.ceil - 5.0);
                self.wf_pending = pending;
                self.wf_pending.fill(f32::MIN);
                self.wf_last = Some(std::time::Instant::now());
            }
            self.db = f.db;
        }
    }

    /// Slide the waterfall to match a new centre frequency.
    fn slide_waterfall(&mut self, bins: usize) {
        if bins == 0 {
            return;
        }
        let hz_per_bin = self.rate / bins as f64;
        let d = ((self.center - self.wf_center) / hz_per_bin).round();
        if d != 0.0 {
            self.wf.shift(d as i32);
            self.wf_center += d * hz_per_bin;
        }
    }

    /// Track percentiles, not extremes: one strong carrier would otherwise
    /// flatten everything else in the span.
    fn rescale(&mut self, db: &[f32]) {
        let mut v: Vec<f32> = db.iter().copied().filter(|x| x.is_finite()).collect();
        if v.is_empty() {
            return;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let pct = |p: f32| v[((v.len() - 1) as f32 * p) as usize];
        let (lo, hi) = (pct(0.10) - 6.0, pct(0.999) + 3.0);
        self.floor += (lo - self.floor) * 0.05;
        self.ceil += (hi.max(lo + 20.0) - self.ceil) * 0.05;
    }

    fn hz_at(&self, rect: &Rect, x: f32) -> f64 {
        let t = ((x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64;
        self.center - self.rate / 2.0 + t * self.rate
    }

    fn x_of(&self, rect: &Rect, hz: f64) -> f32 {
        let t = (hz - (self.center - self.rate / 2.0)) / self.rate;
        rect.left() + (t as f32) * rect.width()
    }

    fn retune(&mut self, hz: f64) {
        self.center = hz.clamp(24e6, 1766e6);
        self.send(Cmd::Center(Hz(self.center as u64)));
        self.retune_listener();
    }

    /// The span or bin count changed, so old rows no longer line up.
    fn reset_waterfall(&mut self) {
        self.wf.clear();
        self.wf_center = self.center;
        self.wf_pending.clear();
    }

    fn add_channel(&mut self, freq: f64) {
        let id = self.next_id;
        self.next_id += 1;
        self.channels.push(Channel {
            freq,
            demod: bands::demod_at(freq),
            label: format!("CH{id}"),
        });
        self.listen(self.channels.len() - 1);
    }

    fn listen(&mut self, idx: usize) {
        let Some(ch) = self.channels.get(idx) else { return };
        let (freq, demod) = (ch.freq, ch.demod);
        self.listening = Some(idx);
        self.send(Cmd::Demod(demod));
        self.send(Cmd::Listen(Some(freq - self.center)));
        self.send(Cmd::Volume(self.volume));
    }

    fn retune_listener(&mut self) {
        match self.listening {
            Some(i) if i < self.channels.len() => self.listen(i),
            _ => {
                self.listening = None;
                self.send(Cmd::Listen(None));
            }
        }
    }
}

fn fmt_hz(hz: f64) -> String {
    if hz.abs() >= 1e6 {
        format!("{:.4} MHz", hz / 1e6)
    } else if hz.abs() >= 1e3 {
        format!("{:.1} kHz", hz / 1e3)
    } else {
        format!("{hz:.0} Hz")
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _f: &mut eframe::Frame) {
        let _f = tracing::info_span!("frame").entered();
        {
            let _s = tracing::info_span!("drain").entered();
            self.drain();
        }
        self.screenshot(ui.ctx());
        self.soak_check(ui.ctx());
        {
            let _s = tracing::info_span!("head").entered();
            self.head(ui);
        }
        {
            let _s = tracing::info_span!("strip").entered();
            self.strip(ui);
        }
        let _s = tracing::info_span!("scope").entered();
        CentralPanel::default()
            .frame(egui::Frame::NONE.fill(theme::CHASSIS))
            .show(ui, |ui| self.scope(ui));
    }
}

impl App {
    /// Self-measured CPU, because a GUI process is awkward to sample from a
    /// shell and the number that matters is over a steady-state window.
    fn soak_check(&mut self, ctx: &egui::Context) {
        let Some(secs) = self.soak else { return };
        if self.shot_sent {
            return;
        }
        // Deliberately does not request repaints: the point is to measure how
        // often the app redraws on its own.
        let t0 = *self.shot_at.get_or_insert_with(std::time::Instant::now);
        let el = t0.elapsed().as_secs_f32();
        if el < secs {
            return;
        }
        let cpu = std::fs::read_to_string("/proc/self/stat")
            .ok()
            .and_then(|s| {
                // Fields are offset by the comm field, which can contain
                // spaces and parentheses, so start after the last ')'.
                let tail = &s[s.rfind(')')? + 1..];
                let f: Vec<&str> = tail.split_whitespace().collect();
                let u: f64 = f.get(11)?.parse().ok()?;
                let k: f64 = f.get(12)?.parse().ok()?;
                Some((u + k) / 100.0)
            })
            .unwrap_or(0.0);
        println!(
            "ran {el:.1}s, used {cpu:.2}s CPU = {:.0}% of one core",
            cpu / el as f64 * 100.0
        );
        crate::prof::report(std::time::Duration::from_secs_f32(el));
        self.shot_sent = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn screenshot(&mut self, ctx: &egui::Context) {
        let Some(path) = self.shot.clone() else { return };
        ctx.request_repaint();
        let t0 = *self.shot_at.get_or_insert_with(std::time::Instant::now);
        // Wait for the tuner to lock and the waterfall to fill; a screenshot
        // taken before that reviews an empty screen, not the design.
        if !self.shot_sent && t0.elapsed().as_secs_f32() > 6.0 {
            if self.channels.is_empty() {
                self.add_channel(95.8e6);
                self.add_channel(95.35e6);
            }
            self.shot_sent = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
        let img = ctx.input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = img {
            let (w, h) = (img.width() as u32, img.height() as u32);
            let buf: Vec<u8> = img.pixels.iter().flat_map(|p| [p.r(), p.g(), p.b(), p.a()]).collect();
            if let Some(b) = image::RgbaImage::from_raw(w, h, buf) {
                let _ = b.save(&path);
                println!("wrote {path} ({w}x{h})");
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// The readout and the controls that set it.
    fn head(&mut self, ui: &mut egui::Ui) {
        Panel::top("head")
            .frame(
                egui::Frame::NONE
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin::symmetric(14, 10)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let out = self.dial.show(ui, self.center, 34.0);
                    if out.changed {
                        self.retune(out.hz);
                    }

                    ui.add_space(16.0);
                    ui.vertical(|ui| {
                        ui.add_space(2.0);
                        ui.label(legend("band"));
                        ui.label(
                            value(bands::name_at(self.center))
                                .color(theme::TRACE)
                                .size(14.0),
                        );
                    });

                    ui.add_space(18.0);
                    self.divider(ui);
                    ui.add_space(18.0);

                    ui.vertical(|ui| {
                        ui.label(legend("span"));
                        ui.horizontal(|ui| {
                            for (name, r) in SPANS {
                                let on = (self.rate - r).abs() < 1.0;
                                if ui.selectable_label(on, name).clicked() && !on {
                                    self.rate = r;
                                    self.send(Cmd::Rate(Sps(r as u64)));
                                    self.reset_waterfall();
                                    self.retune_listener();
                                }
                            }
                        });
                    });

                    ui.add_space(18.0);
                    self.divider(ui);
                    ui.add_space(18.0);

                    ui.vertical(|ui| {
                        ui.label(legend("gain"));
                        ui.horizontal(|ui| {
                            let auto = matches!(self.gain, GainMode::Auto);
                            if ui.selectable_label(auto, "AUTO").clicked() && !auto {
                                self.gain = GainMode::Auto;
                                self.send(Cmd::Gain(self.gain));
                            }
                            if ui.selectable_label(!auto, "MAN").clicked() && auto {
                                self.gain = GainMode::Manual(30.0);
                                self.send(Cmd::Gain(self.gain));
                            }
                            if let GainMode::Manual(mut g) = self.gain {
                                if ui
                                    .add(
                                        egui::Slider::new(&mut g, 0.0..=50.0)
                                            .suffix(" dB")
                                            .show_value(true),
                                    )
                                    .changed()
                                {
                                    self.gain = GainMode::Manual(g);
                                    self.send(Cmd::Gain(self.gain));
                                }
                            }
                        });
                    });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        self.lamps(ui);
                    });
                });

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label(legend("fft bins"));
                    for n in FFTS {
                        let on = self.fft_size == n;
                        if ui.selectable_label(on, n.to_string()).clicked() && !on {
                            self.fft_size = n;
                            self.send(Cmd::Fft(n));
                            self.reset_waterfall();
                        }
                    }

                    ui.add_space(14.0);
                    ui.label(legend("rows/sec"));
                    for (name, v) in SPEEDS {
                        let on = (self.rows_per_sec - v).abs() < 0.01;
                        if ui.selectable_label(on, name).clicked() {
                            self.rows_per_sec = v;
                        }
                    }

                    ui.add_space(14.0);
                    ui.label(legend("fps"));
                    for (name, v) in REFRESH {
                        let on = (self.refresh - v).abs() < 0.01;
                        if ui.selectable_label(on, name).clicked() && !on {
                            self.refresh = v;
                            self.send(Cmd::Refresh(v));
                        }
                    }

                    ui.add_space(14.0);
                    ui.label(legend("scale"));
                    if ui.selectable_label(self.auto_scale, "AUTO").clicked() {
                        self.auto_scale = !self.auto_scale;
                    }
                    if !self.auto_scale {
                        ui.add(egui::Slider::new(&mut self.floor, -140.0..=0.0).text("floor"));
                        ui.add(egui::Slider::new(&mut self.ceil, -140.0..=20.0).text("ceil"));
                    }
                });

                if let Some(e) = &self.err {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(e)
                            .color(theme::FAULT)
                            .font(FontId::proportional(12.0)),
                    );
                }
            });
    }

    fn divider(&self, ui: &mut egui::Ui) {
        let h = 40.0;
        let (rect, _) = ui.allocate_exact_size(Vec2::new(1.0, h), Sense::hover());
        ui.painter().line_segment(
            [
                Pos2::new(rect.center().x, rect.top()),
                Pos2::new(rect.center().x, rect.bottom()),
            ],
            Stroke::new(1.0, theme::ETCH),
        );
    }

    /// Status lamps. Dark is good; an unlit lamp means nothing is wrong.
    fn lamps(&self, ui: &mut egui::Ui) {
        let Some(r) = &self.radio else { return };
        use std::sync::atomic::Ordering;
        let dropped = r.status.dropped.load(Ordering::Relaxed);
        let running = r.status.running.load(Ordering::Relaxed);

        ui.vertical(|ui| {
            ui.add_space(4.0);
            lamp(ui, "drops", dropped > 0, theme::FAULT, &format!("{dropped}"));
            lamp(ui, "rx", running, theme::TRACE, if running { "on" } else { "off" });
        });
    }

    fn strip(&mut self, ui: &mut egui::Ui) {
        Panel::right("channels")
            .default_size(250.0)
            .frame(
                egui::Frame::NONE
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin::symmetric(12, 10)),
            )
            .show(ui, |ui| {
                ui.label(legend("channels"));
                ui.add_space(6.0);

                ui.horizontal(|ui| {
                    ui.label(legend("vol"));
                    if ui
                        .add(egui::Slider::new(&mut self.volume, 0.0..=1.0).show_value(false))
                        .changed()
                    {
                        self.send(Cmd::Volume(self.volume));
                    }
                    if ui.button("MUTE").clicked() {
                        self.listening = None;
                        self.send(Cmd::Listen(None));
                    }
                });

                ui.add_space(8.0);

                if self.channels.is_empty() {
                    ui.label(
                        egui::RichText::new("Click the spectrum to tune a channel.")
                            .color(theme::LEGEND)
                            .size(12.0),
                    );
                }

                let mut remove = None;
                let mut tune = None;
                for (i, ch) in self.channels.iter_mut().enumerate() {
                    let active = self.listening == Some(i);
                    egui::Frame::NONE
                        .fill(if active { Color32::from_rgb(0x2A, 0x2E, 0x36) } else { theme::WELL })
                        .stroke(Stroke::new(1.0, if active { theme::READOUT } else { theme::ETCH }))
                        .corner_radius(2.0)
                        .inner_margin(egui::Margin::same(8))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                // A lit bar marks the channel you are hearing.
                                let (r, _) = ui.allocate_exact_size(Vec2::new(3.0, 16.0), Sense::hover());
                                ui.painter().rect_filled(
                                    r,
                                    1.0,
                                    if active { theme::READOUT } else { theme::ETCH },
                                );
                                ui.add(
                                    egui::TextEdit::singleline(&mut ch.label)
                                        .desired_width(90.0)
                                        .frame(egui::Frame::NONE),
                                );
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        if ui.small_button("REMOVE").clicked() {
                                            remove = Some(i);
                                        }
                                    },
                                );
                            });
                            ui.label(readout(format!("{:.4}", ch.freq / 1e6), 17.0));
                            ui.label(legend(bands::name_at(ch.freq)));
                            ui.add_space(4.0);
                            ui.horizontal(|ui| {
                                for m in [Demod::Wfm, Demod::Nfm, Demod::Am] {
                                    if ui.selectable_label(ch.demod == m, m.label()).clicked() {
                                        ch.demod = m;
                                        tune = Some(i);
                                    }
                                }
                                if !active
                                    && ui.small_button("LISTEN").clicked()
                                {
                                    tune = Some(i);
                                }
                            });
                        });
                    ui.add_space(6.0);
                }

                if let Some(i) = remove {
                    self.channels.remove(i);
                    match self.listening {
                        Some(l) if l == i => self.listening = None,
                        Some(l) if l > i => self.listening = Some(l - 1),
                        _ => {}
                    }
                    self.retune_listener();
                }
                if let Some(i) = tune {
                    self.listen(i);
                }

            });
    }

    fn scope(&mut self, ui: &mut egui::Ui) {
        let full = ui.available_rect_before_wrap();
        let ribbon_h = 16.0;
        let plot_h = (full.height() - ribbon_h) * 0.34;
        let plot = Rect::from_min_max(full.min, Pos2::new(full.right(), full.top() + plot_h));
        let ribbon = Rect::from_min_max(
            Pos2::new(full.left(), plot.bottom()),
            Pos2::new(full.right(), plot.bottom() + ribbon_h),
        );
        let fall = Rect::from_min_max(Pos2::new(full.left(), ribbon.bottom()), full.max);

        let resp = ui.allocate_rect(full, Sense::click_and_drag());
        let p = ui.painter_at(full).to_owned();
        p.rect_filled(plot, 0.0, theme::WELL);

        self.grid(&p, &plot);
        {
            let _s = tracing::info_span!("trace").entered();
            self.trace(&p, &plot);
        }
        self.ribbon(&p, &ribbon);

        p.rect_filled(fall, 0.0, theme::CHASSIS);
        {
            let _wf = tracing::info_span!("wf_texture").entered();
            self.wf.draw(ui.ctx(), &p, fall);
        }

        self.markers(&p, &full);
        self.cursor(&p, &full, &resp);

        if resp.clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                let hz = self.hz_at(&full, pos.x);
                let tol = self.rate / full.width() as f64 * 6.0;
                match self.channels.iter().position(|c| (c.freq - hz).abs() < tol) {
                    Some(i) => self.listen(i),
                    None => self.add_channel(hz),
                }
            }
        }
        if resp.dragged() {
            let dx = resp.drag_delta().x as f64;
            if dx.abs() > 0.0 {
                self.retune(self.center - dx * self.rate / full.width() as f64);
            }
        }

        // Wheel over the pane scrubs the centre frequency. A notch moves a
        // twentieth of the span, so the gesture means the same thing at every
        // zoom level.
        if resp.hovered() {
            let n = self.scrub.notches(ui);
            if n != 0 {
                self.retune(self.center - n as f64 * self.rate / 20.0);
            }
        }
    }

    fn grid(&self, p: &egui::Painter, plot: &Rect) {
        for i in 0..=10 {
            let x = plot.left() + plot.width() * i as f32 / 10.0;
            p.line_segment(
                [Pos2::new(x, plot.top()), Pos2::new(x, plot.bottom())],
                Stroke::new(1.0, Color32::from_rgb(0x24, 0x28, 0x2E)),
            );
        }
        // Amplitude graticule, labelled in dBFS so the numbers mean something.
        for i in 1..4 {
            let y = plot.top() + plot.height() * i as f32 / 4.0;
            p.line_segment(
                [Pos2::new(plot.left(), y), Pos2::new(plot.right(), y)],
                Stroke::new(1.0, Color32::from_rgb(0x22, 0x26, 0x2B)),
            );
            let db = self.ceil - (self.ceil - self.floor) * i as f32 / 4.0;
            p.text(
                Pos2::new(plot.right() - 4.0, y - 1.0),
                Align2::RIGHT_BOTTOM,
                format!("{db:.0}"),
                FontId::new(9.0, FontFamily::Name(theme::LEGEND_FONT.into())),
                Color32::from_rgb(0x4A, 0x51, 0x5A),
            );
        }
    }

    fn trace(&self, p: &egui::Painter, plot: &Rect) {
        if self.db.is_empty() {
            return;
        }
        let span = (self.ceil - self.floor).max(1.0);
        let n = self.db.len();
        let cols = plot.width().max(1.0) as usize;
        let mut pts = Vec::with_capacity(cols);
        for c in 0..cols {
            let a = c * n / cols;
            let b = ((c + 1) * n / cols).max(a + 1).min(n);
            // Max, not mean: averaging hides the narrow carriers that matter.
            let v = self.db[a..b].iter().copied().fold(f32::MIN, f32::max);
            let t = ((v - self.floor) / span).clamp(0.0, 1.0);
            pts.push(Pos2::new(plot.left() + c as f32, plot.bottom() - t * plot.height()));
        }
        // Fill under the trace so occupied spectrum reads as mass. Built as a
        // quad strip: a spectrum outline is wildly concave, and asking for a
        // convex polygon fill turns it into fan-shaped wedges.
        let mut mesh = egui::Mesh::default();
        let fill = Color32::from_rgba_unmultiplied(0x5C, 0xD0, 0xE8, 26);
        for w in pts.windows(2) {
            let i = mesh.vertices.len() as u32;
            for v in [
                w[0],
                w[1],
                Pos2::new(w[1].x, plot.bottom()),
                Pos2::new(w[0].x, plot.bottom()),
            ] {
                mesh.colored_vertex(v, fill);
            }
            mesh.add_triangle(i, i + 1, i + 2);
            mesh.add_triangle(i, i + 2, i + 3);
        }
        p.add(egui::Shape::mesh(mesh));
        p.add(egui::Shape::line(pts, Stroke::new(1.2, theme::TRACE)));
    }

    fn ribbon(&self, p: &egui::Painter, r: &Rect) {
        p.rect_filled(*r, 0.0, theme::CHASSIS);
        let (lo, hi) = (self.center - self.rate / 2.0, self.center + self.rate / 2.0);
        for b in bands::in_span(lo, hi) {
            let x0 = self.x_of(r, b.lo.max(lo)).max(r.left());
            let x1 = self.x_of(r, b.hi.min(hi)).min(r.right());
            if x1 - x0 < 1.0 {
                continue;
            }
            let cell = Rect::from_min_max(Pos2::new(x0, r.top() + 2.0), Pos2::new(x1, r.bottom() - 2.0));
            p.rect_filled(cell, 1.0, b.color);
            if x1 - x0 > 60.0 {
                p.text(
                    cell.center(),
                    Align2::CENTER_CENTER,
                    b.name,
                    FontId::new(9.0, FontFamily::Name(theme::LEGEND_FONT.into())),
                    Color32::from_rgb(0xE8, 0xEC, 0xF0),
                );
            }
        }
    }

    fn markers(&self, p: &egui::Painter, full: &Rect) {
        let (lo, hi) = (self.center - self.rate / 2.0, self.center + self.rate / 2.0);
        for (i, ch) in self.channels.iter().enumerate() {
            if ch.freq < lo || ch.freq > hi {
                continue;
            }
            let x = self.x_of(full, ch.freq);
            let active = self.listening == Some(i);
            let col = if active { theme::READOUT } else { Color32::from_rgb(0x6E, 0x7A, 0x88) };

            // Show what the demodulator actually takes in, not just where it
            // is centred: an NFM channel and a WFM channel at the same spot
            // are wildly different slices of spectrum.
            let half = ch.demod.bandwidth() / 2.0;
            let (bx0, bx1) = (self.x_of(full, ch.freq - half), self.x_of(full, ch.freq + half));
            if bx1 - bx0 >= 1.0 {
                let band = Rect::from_min_max(
                    Pos2::new(bx0.max(full.left()), full.top()),
                    Pos2::new(bx1.min(full.right()), full.bottom()),
                );
                p.rect_filled(
                    band,
                    0.0,
                    Color32::from_rgba_unmultiplied(col.r(), col.g(), col.b(), if active { 34 } else { 18 }),
                );
                for ex in [bx0, bx1] {
                    if full.x_range().contains(ex) {
                        p.line_segment(
                            [Pos2::new(ex, full.top()), Pos2::new(ex, full.bottom())],
                            Stroke::new(1.0, Color32::from_rgba_unmultiplied(col.r(), col.g(), col.b(), 120)),
                        );
                    }
                }
            }
            p.line_segment(
                [Pos2::new(x, full.top()), Pos2::new(x, full.bottom())],
                Stroke::new(if active { 1.5 } else { 1.0 }, col),
            );
            // Flag the label off the line so it never sits on the trace.
            let t = p.layout_no_wrap(
                ch.label.clone(),
                FontId::new(10.0, FontFamily::Name(theme::LEGEND_FONT.into())),
                Color32::BLACK,
            );
            let flag = Rect::from_min_size(
                Pos2::new(x + 1.0, full.top() + 2.0),
                Vec2::new(t.size().x + 8.0, t.size().y + 4.0),
            );
            p.rect_filled(flag, 1.0, col);
            p.galley(Pos2::new(flag.left() + 4.0, flag.top() + 2.0), t, Color32::BLACK);
        }
    }

    fn cursor(&self, p: &egui::Painter, full: &Rect, resp: &egui::Response) {
        let Some(pos) = resp.hover_pos() else { return };
        let hz = self.hz_at(full, pos.x);
        p.line_segment(
            [Pos2::new(pos.x, full.top()), Pos2::new(pos.x, full.bottom())],
            Stroke::new(1.0, Color32::from_rgb(0x55, 0x5E, 0x69)),
        );
        let text = format!("{}   {}", fmt_hz(hz), bands::name_at(hz));
        let g = p.layout_no_wrap(
            text,
            FontId::new(11.0, FontFamily::Name(theme::READOUT_FONT.into())),
            theme::VALUE,
        );
        let left = (pos.x + 8.0).min(full.right() - g.size().x - 10.0);
        let box_r = Rect::from_min_size(
            Pos2::new(left - 5.0, full.top() + 5.0),
            g.size() + Vec2::new(10.0, 6.0),
        );
        p.rect(box_r, 2.0, theme::WELL, Stroke::new(1.0, theme::ETCH), StrokeKind::Inside);
        p.galley(Pos2::new(left, full.top() + 8.0), g, theme::VALUE);
    }
}

fn lamp(ui: &mut egui::Ui, label: &str, lit: bool, col: Color32, text: &str) {
    ui.horizontal(|ui| {
        let (r, _) = ui.allocate_exact_size(Vec2::new(7.0, 7.0), Sense::hover());
        let c = if lit { col } else { Color32::from_rgb(0x2C, 0x31, 0x38) };
        ui.painter().circle_filled(r.center(), 3.5, c);
        if lit {
            // A faint halo reads as a lit lamp rather than a painted dot.
            ui.painter()
                .circle_filled(r.center(), 6.0, Color32::from_rgba_unmultiplied(col.r(), col.g(), col.b(), 30));
        }
        ui.label(legend(label));
        ui.label(value(text).size(11.0).color(if lit { col } else { theme::LEGEND }));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        App { center: 100_000_000.0, rate: 2_000_000.0, ..Default::default() }
    }

    fn rect() -> Rect {
        Rect::from_min_size(Pos2::new(0.0, 0.0), Vec2::new(1000.0, 400.0))
    }

    #[test]
    fn frequency_mapping_round_trips() {
        let a = app();
        let r = rect();
        for hz in [99_000_000.0, 100_000_000.0, 100_750_000.0] {
            let back = a.hz_at(&r, a.x_of(&r, hz));
            assert!((back - hz).abs() < 1.0, "{hz} came back as {back}");
        }
    }

    #[test]
    fn the_edges_are_the_ends_of_the_span() {
        let a = app();
        let r = rect();
        assert!((a.hz_at(&r, r.left()) - 99_000_000.0).abs() < 1.0);
        assert!((a.hz_at(&r, r.right()) - 101_000_000.0).abs() < 1.0);
    }

    #[test]
    fn clicks_outside_the_pane_clamp_to_the_span() {
        let a = app();
        let r = rect();
        assert!((a.hz_at(&r, -500.0) - 99_000_000.0).abs() < 1.0);
        assert!((a.hz_at(&r, 5000.0) - 101_000_000.0).abs() < 1.0);
    }

    #[test]
    fn new_channels_take_the_mode_of_their_band() {
        let mut a = app();
        a.add_channel(95.8e6);
        a.add_channel(124.0e6);
        assert_eq!(a.channels[0].demod, Demod::Wfm);
        assert_eq!(a.channels[1].demod, Demod::Am);
    }

    #[test]
    fn auto_scale_ignores_a_single_strong_carrier() {
        let mut a = app();
        a.floor = -90.0;
        a.ceil = -20.0;
        let mut db = vec![-95.0f32; 1024];
        db[500] = 0.0;
        for _ in 0..200 {
            a.rescale(&db);
        }
        assert!(a.floor < -95.0, "floor tracked the carrier: {}", a.floor);
        assert!(a.floor > -110.0, "floor ran away: {}", a.floor);
    }

    #[test]
    fn retuning_stays_inside_what_the_tuner_can_reach() {
        let mut a = app();
        a.retune(1.0);
        assert_eq!(a.center, 24e6);
        a.retune(9e9);
        assert_eq!(a.center, 1766e6);
    }

    #[test]
    fn fmt_hz_scales_units() {
        assert_eq!(fmt_hz(95_800_000.0), "95.8000 MHz");
        assert_eq!(fmt_hz(12_500.0), "12.5 kHz");
        assert_eq!(fmt_hz(400.0), "400 Hz");
    }
}

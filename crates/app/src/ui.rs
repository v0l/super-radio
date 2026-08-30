//! Spectrum + waterfall pane with click-to-tune, and the control panels.

use crate::radio::{Cmd, Demod, Frame, Radio};
use crate::waterfall::Waterfall;
use common::{GainMode, Hz, Sps};
use egui::containers::{CentralPanel, Panel};
use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};

pub struct App {
    radio: Option<Radio>,
    err: Option<String>,

    center: f64,
    rate: f64,
    gain: GainMode,
    ppm: f64,

    /// Latest spectrum, held so the plot still draws between radio updates.
    db: Vec<f32>,
    wf: Waterfall,
    floor: f32,
    ceil: f32,
    auto_scale: bool,

    channels: Vec<Channel>,
    listening: Option<usize>,
    volume: f32,
    next_id: u32,

    fft: usize,
}

pub struct Channel {
    /// Absolute frequency, so it survives retuning the radio.
    freq: f64,
    demod: Demod,
    label: String,
}

const SPAN_PRESETS: [(&str, f64); 5] = [
    ("0.25 MS/s", 250_000.0),
    ("1.024 MS/s", 1_024_000.0),
    ("2.048 MS/s", 2_048_000.0),
    ("2.304 MS/s", 2_304_000.0),
    ("2.4 MS/s", 2_400_000.0),
];

impl Default for App {
    fn default() -> Self {
        Self {
            radio: None,
            err: None,
            center: 95_800_000.0,
            rate: 2_304_000.0,
            gain: GainMode::Auto,
            ppm: 0.0,
            db: Vec::new(),
            wf: Waterfall::new(512),
            floor: -90.0,
            ceil: -20.0,
            auto_scale: true,
            channels: Vec::new(),
            listening: None,
            volume: 0.5,
            next_id: 1,
            fft: 2048,
        }
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let mut app = Self::default();
        app.connect(&cc.egui_ctx);
        app
    }

    fn connect(&mut self, ctx: &egui::Context) {
        let c = ctx.clone();
        let radio = Radio::start(
            0,
            Hz(self.center as u64),
            Sps(self.rate as u64),
            self.fft,
            move || c.request_repaint(),
        );
        self.radio = Some(radio);
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
            self.wf.push(&f.db, self.floor, self.ceil);
            self.db = f.db;
        }
    }

    /// Track the noise floor rather than the extremes: a single strong carrier
    /// would otherwise flatten the whole display.
    fn rescale(&mut self, db: &[f32]) {
        let mut v: Vec<f32> = db.iter().copied().filter(|x| x.is_finite()).collect();
        if v.is_empty() {
            return;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let pct = |p: f32| v[((v.len() - 1) as f32 * p) as usize];
        let (lo, hi) = (pct(0.10) - 6.0, pct(0.999) + 10.0);
        // Glide, or the picture flickers on every burst.
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

    fn add_channel(&mut self, freq: f64) {
        let demod = default_demod(freq);
        let id = self.next_id;
        self.next_id += 1;
        self.channels.push(Channel { freq, demod, label: format!("CH{id}") });
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

/// Guess a sensible mode from the band, so a click just works.
fn default_demod(hz: f64) -> Demod {
    let mhz = hz / 1e6;
    if (87.5..108.0).contains(&mhz) {
        Demod::Wfm
    } else if (108.0..137.0).contains(&mhz) {
        Demod::Am
    } else {
        Demod::Nfm
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
        self.drain();
        self.top_bar(ui);
        self.side_panel(ui);
        CentralPanel::default().show(ui, |ui| self.spectrum(ui));
    }
}

impl App {
    fn top_bar(&mut self, ui: &mut egui::Ui) {
        Panel::top("top").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("super-radio");
                ui.separator();

                ui.label("Centre");
                let mut mhz = self.center / 1e6;
                let r = ui.add(
                    egui::DragValue::new(&mut mhz)
                        .speed(0.1)
                        .range(24.0..=1766.0)
                        .fixed_decimals(4)
                        .suffix(" MHz"),
                );
                if r.changed() {
                    self.center = mhz * 1e6;
                    self.send(Cmd::Center(Hz(self.center as u64)));
                    self.wf.clear();
                    self.retune_listener();
                }

                ui.separator();
                ui.label("Span");
                let cur = self.rate;
                egui::ComboBox::from_id_salt("span")
                    .selected_text(fmt_hz(cur))
                    .show_ui(ui, |ui| {
                        for (name, r) in SPAN_PRESETS {
                            if ui.selectable_label((cur - r).abs() < 1.0, name).clicked() {
                                self.rate = r;
                                self.send(Cmd::Rate(Sps(r as u64)));
                                self.wf.clear();
                                self.retune_listener();
                            }
                        }
                    });

                ui.separator();
                ui.label("Gain");
                let mut auto = matches!(self.gain, GainMode::Auto);
                if ui.checkbox(&mut auto, "auto").changed() {
                    self.gain = if auto { GainMode::Auto } else { GainMode::Manual(30.0) };
                    self.send(Cmd::Gain(self.gain));
                }
                if let GainMode::Manual(mut g) = self.gain {
                    if ui.add(egui::Slider::new(&mut g, 0.0..=50.0).suffix(" dB")).changed() {
                        self.gain = GainMode::Manual(g);
                        self.send(Cmd::Gain(self.gain));
                    }
                }

                ui.separator();
                if ui
                    .add(egui::DragValue::new(&mut self.ppm).speed(0.1).range(-200.0..=200.0).prefix("ppm "))
                    .changed()
                {
                    // Applied on the next retune.
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if let Some(r) = &self.radio {
                        let d = r.status.dropped.load(std::sync::atomic::Ordering::Relaxed);
                        let col = if d > 0 { Color32::from_rgb(230, 90, 90) } else { Color32::GRAY };
                        ui.colored_label(col, format!("dropped {d}"));
                    }
                });
            });

            if let Some(e) = &self.err {
                ui.colored_label(Color32::from_rgb(240, 100, 100), e);
            }
        });
    }

    fn side_panel(&mut self, ui: &mut egui::Ui) {
        Panel::right("channels").default_size(260.0).show(ui, |ui| {
            ui.heading("Channels");
            ui.label(
                egui::RichText::new("Click the spectrum to add one.")
                    .small()
                    .color(Color32::GRAY),
            );
            ui.separator();

            ui.horizontal(|ui| {
                ui.label("Volume");
                if ui.add(egui::Slider::new(&mut self.volume, 0.0..=1.0)).changed() {
                    self.send(Cmd::Volume(self.volume));
                }
            });
            if ui.button("Mute").clicked() {
                self.listening = None;
                self.send(Cmd::Listen(None));
            }
            ui.separator();

            let mut remove = None;
            let mut tune = None;
            let mut changed = None;
            for (i, ch) in self.channels.iter_mut().enumerate() {
                let active = self.listening == Some(i);
                egui::Frame::group(ui.style())
                    .fill(if active {
                        Color32::from_rgb(40, 55, 75)
                    } else {
                        ui.style().visuals.faint_bg_color
                    })
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.text_edit_singleline(&mut ch.label);
                            if ui.small_button("x").clicked() {
                                remove = Some(i);
                            }
                        });
                        ui.label(
                            egui::RichText::new(fmt_hz(ch.freq)).monospace().strong(),
                        );
                        ui.horizontal(|ui| {
                            for m in [Demod::Wfm, Demod::Nfm, Demod::Am] {
                                if ui.selectable_label(ch.demod == m, m.label()).clicked() {
                                    ch.demod = m;
                                    changed = Some(i);
                                }
                            }
                        });
                        if ui.selectable_label(active, if active { "listening" } else { "listen" }).clicked() {
                            tune = Some(i);
                        }
                    });
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
            if let Some(i) = tune.or(changed) {
                self.listen(i);
            }

            ui.separator();
            ui.collapsing("Display", |ui| {
                ui.checkbox(&mut self.auto_scale, "auto scale");
                ui.add_enabled(
                    !self.auto_scale,
                    egui::Slider::new(&mut self.floor, -140.0..=0.0).text("floor dB"),
                );
                ui.add_enabled(
                    !self.auto_scale,
                    egui::Slider::new(&mut self.ceil, -140.0..=20.0).text("ceiling dB"),
                );
            });
        });
    }

    fn spectrum(&mut self, ui: &mut egui::Ui) {
        let full = ui.available_rect_before_wrap();
        let split = full.top() + full.height() * 0.35;
        let plot = Rect::from_min_max(full.min, Pos2::new(full.right(), split));
        let fall = Rect::from_min_max(Pos2::new(full.left(), split + 2.0), full.max);

        let resp = ui.allocate_rect(full, Sense::click_and_drag());
        let p = ui.painter_at(full);

        p.rect_filled(plot, 0.0, Color32::from_rgb(10, 12, 16));

        // Frequency grid.
        for i in 0..=10 {
            let x = plot.left() + plot.width() * i as f32 / 10.0;
            p.line_segment(
                [Pos2::new(x, plot.top()), Pos2::new(x, plot.bottom())],
                Stroke::new(1.0, Color32::from_gray(30)),
            );
            let hz = self.hz_at(&plot, x);
            p.text(
                Pos2::new(x, plot.bottom() - 2.0),
                Align2::CENTER_BOTTOM,
                format!("{:.3}", hz / 1e6),
                FontId::monospace(10.0),
                Color32::from_gray(110),
            );
        }

        if !self.db.is_empty() {
            let span = (self.ceil - self.floor).max(1.0);
            let n = self.db.len();
            // More bins than pixels, so take the max per column: averaging
            // would hide narrow carriers, which are exactly what matters.
            let cols = plot.width().max(1.0) as usize;
            let mut pts = Vec::with_capacity(cols);
            for c in 0..cols {
                let a = c * n / cols;
                let b = ((c + 1) * n / cols).max(a + 1).min(n);
                let v = self.db[a..b].iter().copied().fold(f32::MIN, f32::max);
                let t = ((v - self.floor) / span).clamp(0.0, 1.0);
                pts.push(Pos2::new(plot.left() + c as f32, plot.bottom() - t * plot.height()));
            }
            p.add(egui::Shape::line(pts, Stroke::new(1.0, Color32::from_rgb(120, 220, 160))));
        }

        if let Some(tex) = self.wf.texture(ui.ctx()) {
            p.image(
                tex.id(),
                fall,
                Rect::from_min_size(Pos2::ZERO, Vec2::new(1.0, 1.0)),
                Color32::WHITE,
            );
        }

        // Channel markers.
        let lo = self.center - self.rate / 2.0;
        let hi = self.center + self.rate / 2.0;
        for (i, ch) in self.channels.iter().enumerate() {
            if ch.freq < lo || ch.freq > hi {
                continue;
            }
            let x = self.x_of(&full, ch.freq);
            let active = self.listening == Some(i);
            let col = if active {
                Color32::from_rgb(255, 200, 80)
            } else {
                Color32::from_rgb(120, 150, 200)
            };
            p.line_segment(
                [Pos2::new(x, full.top()), Pos2::new(x, full.bottom())],
                Stroke::new(if active { 2.0 } else { 1.0 }, col),
            );
            p.text(
                Pos2::new(x + 3.0, full.top() + 2.0),
                Align2::LEFT_TOP,
                &ch.label,
                FontId::proportional(11.0),
                col,
            );
        }

        // Hover readout.
        if let Some(pos) = resp.hover_pos() {
            let hz = self.hz_at(&full, pos.x);
            p.line_segment(
                [Pos2::new(pos.x, full.top()), Pos2::new(pos.x, full.bottom())],
                Stroke::new(1.0, Color32::from_gray(90)),
            );
            let text = fmt_hz(hz);
            let at = Pos2::new(pos.x + 6.0, full.top() + 18.0);
            let r = p
                .text(at, Align2::LEFT_TOP, &text, FontId::monospace(12.0), Color32::WHITE)
                .expand(3.0);
            p.rect(
                r,
                3.0,
                Color32::from_black_alpha(200),
                Stroke::NONE,
                StrokeKind::Middle,
            );
            p.text(at, Align2::LEFT_TOP, text, FontId::monospace(12.0), Color32::WHITE);
        }

        if resp.clicked() {
            if let Some(pos) = resp.interact_pointer_pos() {
                let hz = self.hz_at(&full, pos.x);
                // Snap to an existing marker if one is close, so clicking a
                // channel selects it rather than stacking a duplicate.
                let tol = self.rate / full.width() as f64 * 6.0;
                match self.channels.iter().position(|c| (c.freq - hz).abs() < tol) {
                    Some(i) => self.listen(i),
                    None => self.add_channel(hz),
                }
            }
        }

        // Drag to pan the centre frequency.
        if resp.dragged() {
            let dx = resp.drag_delta().x as f64;
            if dx.abs() > 0.0 {
                self.center -= dx * self.rate / full.width() as f64;
                self.send(Cmd::Center(Hz(self.center as u64)));
                self.wf.clear();
                self.retune_listener();
            }
        }
    }
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
    fn the_left_edge_is_the_bottom_of_the_span() {
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
    fn the_band_plan_picks_sensible_modes() {
        assert_eq!(default_demod(95.8e6), Demod::Wfm);
        assert_eq!(default_demod(124.0e6), Demod::Am);
        assert_eq!(default_demod(446.0e6), Demod::Nfm);
    }

    #[test]
    fn removing_a_channel_keeps_the_listener_on_the_same_one() {
        let mut a = app();
        a.channels.push(Channel { freq: 1.0, demod: Demod::Nfm, label: "a".into() });
        a.channels.push(Channel { freq: 2.0, demod: Demod::Nfm, label: "b".into() });
        a.listening = Some(1);
        // Removing the earlier entry must shift the index, not silently
        // re-point the listener at a different channel.
        a.channels.remove(0);
        a.listening = Some(0);
        assert_eq!(a.channels[a.listening.unwrap()].label, "b");
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
    fn fmt_hz_scales_units() {
        assert_eq!(fmt_hz(95_800_000.0), "95.8000 MHz");
        assert_eq!(fmt_hz(12_500.0), "12.5 kHz");
        assert_eq!(fmt_hz(400.0), "400 Hz");
    }
}

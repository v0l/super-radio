//! Draws the signal chain that is actually running.
//!
//! Read from the built graph, so it cannot describe a chain the radio is not
//! using. A diagram maintained alongside the code would be worth less than
//! nothing: wrong documentation is believed.

use crate::theme;
use egui::{Color32, FontFamily, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};
use pipeline::graph::Topology;
use pipeline::port::{PortKind, StreamSpec};

const BOX_W: f32 = 190.0;
const BOX_H: f32 = 46.0;
const GAP: f32 = 34.0;

fn kind_label(k: PortKind) -> &'static str {
    match k {
        PortKind::Iq => "iq",
        PortKind::Real => "real",
        PortKind::Bytes => "bytes",
        PortKind::Pulses => "pulses",
        PortKind::Soft => "soft",
    }
}

/// Rate as a human reads it, in the unit that suits the magnitude.
fn rate_label(s: &StreamSpec) -> String {
    let r = s.frame_rate();
    let base = if r >= 1e6 {
        format!("{:.3} MS/s", r / 1e6)
    } else if r >= 10e3 {
        format!("{:.1} kS/s", r / 1e3)
    } else {
        format!("{r:.0} S/s")
    };
    if s.channels > 1 {
        format!("{base} x{}", s.channels)
    } else {
        base
    }
}

pub fn draw(ui: &mut egui::Ui, topo: &Topology, latency_ms: f64) {
    let n = topo.nodes.len();
    let height = (n + 1) as f32 * (BOX_H + GAP);
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), height.max(ui.available_height())),
        Sense::hover(),
    );
    let p = ui.painter_at(rect);
    let cx = rect.center().x;
    let mut y = rect.top() + 12.0;

    let mut boxes: Vec<(usize, Rect)> = Vec::new();

    p.text(
        Pos2::new(rect.left() + 12.0, rect.top() + 4.0),
        egui::Align2::LEFT_TOP,
        format!("{} stages   {latency_ms:.1} ms through the chain", topo.nodes.len()),
        FontId::new(10.0, FontFamily::Name(theme::READOUT_FONT.into())),
        theme::LEGEND,
    );
    let src = Rect::from_center_size(Pos2::new(cx, y + BOX_H / 2.0), Vec2::new(BOX_W, BOX_H));
    stage(&p, src, "Source", "device", true);
    edge(&p, src.center_bottom(), y + BOX_H + GAP, &topo.input);
    y += BOX_H + GAP;

    for node in &topo.nodes {
        let r = Rect::from_center_size(Pos2::new(cx, y + BOX_H / 2.0), Vec2::new(BOX_W, BOX_H));
        stage(&p, r, &node.label, &node.kind, false);
        boxes.push((node.outputs.first().map(|(s, _)| *s).unwrap_or(0), r));

        if let Some((_, spec)) = node.outputs.first() {
            let last = std::ptr::eq(node, topo.nodes.last().unwrap());
            if !last {
                edge(&p, r.center_bottom(), y + BOX_H + GAP, spec);
            } else {
                // The last stage's output leaves the graph, so it is labelled
                // where it goes rather than pointed at the next box.
                let t = format!("out  {}  {}", kind_label(spec.kind), rate_label(spec));
                p.text(
                    Pos2::new(cx, r.bottom() + 10.0),
                    egui::Align2::CENTER_TOP,
                    t,
                    FontId::new(10.0, FontFamily::Name(theme::READOUT_FONT.into())),
                    theme::TRACE,
                );
            }
        }

        // A stage with more than one output has branches: RDS off the WFM
        // demodulator, for instance. Drawn to the side so the trunk stays a
        // straight line the eye can follow.
        for (i, (_, spec)) in node.outputs.iter().enumerate().skip(1) {
            let bx = Rect::from_min_size(
                Pos2::new(r.right() + 30.0, r.top()),
                Vec2::new(120.0, BOX_H),
            );
            p.line_segment(
                [r.right_center(), bx.left_center()],
                Stroke::new(1.0, theme::LEGEND),
            );
            stage(&p, bx, &format!("out {i}"), kind_label(spec.kind), false);
        }
        y += BOX_H + GAP;
    }
}

fn stage(p: &egui::Painter, r: Rect, label: &str, kind: &str, source: bool) {
    let fill = if source { theme::WELL } else { theme::PANEL };
    p.rect(r, 3.0, fill, Stroke::new(1.0, theme::ETCH), StrokeKind::Inside);
    p.text(
        Pos2::new(r.center().x, r.top() + 9.0),
        egui::Align2::CENTER_TOP,
        label,
        FontId::new(12.0, FontFamily::Proportional),
        theme::VALUE,
    );
    p.text(
        Pos2::new(r.center().x, r.top() + 26.0),
        egui::Align2::CENTER_TOP,
        kind,
        FontId::new(10.0, FontFamily::Name(theme::READOUT_FONT.into())),
        theme::LEGEND,
    );
}

/// A labelled arrow carrying what the link actually contains.
fn edge(p: &egui::Painter, from: Pos2, to_y: f32, spec: &StreamSpec) {
    let to = Pos2::new(from.x, to_y);
    let col = Color32::from_rgb(0x4A, 0x55, 0x60);
    p.line_segment([from, to], Stroke::new(1.0, col));
    for d in [-4.0, 4.0] {
        p.line_segment([Pos2::new(to.x + d, to.y - 6.0), to], Stroke::new(1.0, col));
    }
    p.text(
        Pos2::new(from.x + 10.0, (from.y + to.y) / 2.0),
        egui::Align2::LEFT_CENTER,
        format!("{}  {}", kind_label(spec.kind), rate_label(spec)),
        FontId::new(10.0, FontFamily::Name(theme::READOUT_FONT.into())),
        theme::LEGEND,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use common::Hz;

    #[test]
    fn a_stereo_stream_says_so_in_its_label() {
        let s = StreamSpec::iq(96_000.0, Hz::mhz(95))
            .with_kind(PortKind::Real)
            .with_rate(48_000.0)
            .with_channels(2);
        // The port rate is 96 kHz but each ear runs at 48 kHz. Showing the port
        // rate would read as an audio chain running twice as fast as it is.
        assert_eq!(rate_label(&s), "48.0 kS/s x2");
    }

    #[test]
    fn rates_are_shown_in_a_unit_that_suits_them() {
        let at = |r: f64| rate_label(&StreamSpec::iq(r, Hz::mhz(95)));
        assert_eq!(at(2_304_000.0), "2.304 MS/s");
        assert_eq!(at(48_000.0), "48.0 kS/s");
            // A symbol rate is not helped by being rounded to 1.2 kS/s, which is
        // why the threshold is 10 kS/s rather than 1.
        assert_eq!(at(1187.5), "1188 S/s");
        assert_eq!(at(4_750.0), "4750 S/s");
    }
}

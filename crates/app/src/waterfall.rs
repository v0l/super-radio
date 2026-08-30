//! Scrolling waterfall.
//!
//! Rows are written into a ring buffer and the texture is uploaded whole each
//! frame. Scrolling by rewriting one row and moving a cursor keeps the cost
//! independent of history depth.

use egui::{Color32, ColorImage, TextureHandle, TextureOptions};

pub struct Waterfall {
    width: usize,
    height: usize,
    /// Ring of rows, newest at `cursor - 1`.
    pixels: Vec<Color32>,
    cursor: usize,
    filled: usize,
    tex: Option<TextureHandle>,
    dirty: bool,
}

impl Waterfall {
    pub fn new(height: usize) -> Self {
        Self {
            width: 0,
            height,
            pixels: Vec::new(),
            cursor: 0,
            filled: 0,
            tex: None,
            dirty: false,
        }
    }

    pub fn clear(&mut self) {
        self.pixels.fill(Color32::BLACK);
        self.cursor = 0;
        self.filled = 0;
        self.dirty = true;
    }

    /// Add one spectrum row, mapping dB through the given display range.
    pub fn push(&mut self, db: &[f32], floor: f32, ceil: f32) {
        if db.is_empty() {
            return;
        }
        if db.len() != self.width {
            self.width = db.len();
            self.pixels = vec![Color32::BLACK; self.width * self.height];
            self.cursor = 0;
            self.filled = 0;
        }
        let span = (ceil - floor).max(1.0);
        let row = self.cursor * self.width;
        for (i, &v) in db.iter().enumerate() {
            self.pixels[row + i] = colormap(((v - floor) / span).clamp(0.0, 1.0));
        }
        self.cursor = (self.cursor + 1) % self.height;
        self.filled = (self.filled + 1).min(self.height);
        self.dirty = true;
    }

    /// Texture with the newest row at the top.
    pub fn texture(&mut self, ctx: &egui::Context) -> Option<&TextureHandle> {
        if self.width == 0 {
            return None;
        }
        if self.dirty || self.tex.is_none() {
            let mut img = ColorImage::filled([self.width, self.height], Color32::BLACK);
            for y in 0..self.height {
                // Walk backwards from the newest row so time runs downward.
                let src = (self.cursor + self.height - 1 - y) % self.height;
                let s = src * self.width;
                let d = y * self.width;
                img.pixels[d..d + self.width].copy_from_slice(&self.pixels[s..s + self.width]);
            }
            match &mut self.tex {
                Some(t) => t.set(img, TextureOptions::LINEAR),
                None => {
                    self.tex = Some(ctx.load_texture("waterfall", img, TextureOptions::LINEAR))
                }
            }
            self.dirty = false;
        }
        self.tex.as_ref()
    }
}

/// Cold cyan for noise, hot amber for signal, white at the top.
///
/// Built from the theme's two accents rather than a stock inferno ramp, so the
/// waterfall says the same thing the rest of the panel does: cyan is what the
/// radio hears, amber is where the energy is. Brightness rises monotonically
/// so features stay readable in greyscale and for colour-deficient viewers.
pub fn colormap(t: f32) -> Color32 {
    const STOPS: [(f32, [f32; 3]); 6] = [
        // Most bins in any span are noise, so the ramp stays dark well past
        // the midpoint. Brightening early spends the whole scale on the noise
        // floor and leaves signals nowhere to go.
        (0.00, [0.031, 0.039, 0.051]),
        (0.35, [0.047, 0.125, 0.161]),
        (0.60, [0.078, 0.376, 0.486]),
        (0.78, [0.180, 0.612, 0.745]),
        (0.90, [0.941, 0.627, 0.188]),
        (1.00, [1.0, 0.965, 0.878]),
    ];
    let t = t.clamp(0.0, 1.0);
    let mut i = 0;
    while i + 2 < STOPS.len() && t > STOPS[i + 1].0 {
        i += 1;
    }
    let (a, b) = (STOPS[i], STOPS[i + 1]);
    let f = ((t - a.0) / (b.0 - a.0)).clamp(0.0, 1.0);
    let c = |x: usize| ((a.1[x] + (b.1[x] - a.1[x]) * f) * 255.0) as u8;
    Color32::from_rgb(c(0), c(1), c(2))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luma(c: Color32) -> f32 {
        0.2126 * c.r() as f32 + 0.7152 * c.g() as f32 + 0.0722 * c.b() as f32
    }

    #[test]
    fn the_colormap_brightens_monotonically() {
        let mut prev = -1.0;
        for i in 0..=100 {
            let l = luma(colormap(i as f32 / 100.0));
            assert!(l >= prev - 1.0, "luma dipped at t={}: {l} after {prev}", i as f32 / 100.0);
            prev = l;
        }
    }

    #[test]
    fn the_colormap_clamps_out_of_range_input() {
        assert_eq!(colormap(-5.0), colormap(0.0));
        assert_eq!(colormap(5.0), colormap(1.0));
    }

    #[test]
    fn a_resize_does_not_panic_and_resets() {
        let mut w = Waterfall::new(8);
        w.push(&[-50.0; 16], -100.0, 0.0);
        w.push(&[-50.0; 32], -100.0, 0.0);
        assert_eq!(w.width, 32);
        assert_eq!(w.filled, 1);
    }

    #[test]
    fn the_ring_wraps_without_growing() {
        let mut w = Waterfall::new(4);
        for _ in 0..100 {
            w.push(&[-30.0; 8], -100.0, 0.0);
        }
        assert_eq!(w.pixels.len(), 8 * 4);
        assert_eq!(w.filled, 4);
    }

    #[test]
    fn a_strong_bin_is_brighter_than_a_weak_one() {
        let strong = colormap((-10.0f32 + 100.0) / 100.0);
        let weak = colormap((-90.0f32 + 100.0) / 100.0);
        assert!(luma(strong) > luma(weak));
    }
}

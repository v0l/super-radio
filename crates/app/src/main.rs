mod bands;
mod dial;
mod theme;
mod radio;
mod ui;
mod waterfall;

/// `--probe <mhz>` runs the radio thread without a window and reports what the
/// waterfall would be drawing, so the signal path can be checked over ssh.
fn probe(mhz: f64) {
    use common::{Hz, Sps};
    let rate = 2_304_000.0;
    let r = radio::Radio::start(0, Hz((mhz * 1e6) as u64), Sps(rate as u64), 2048, || {});
    let start = std::time::Instant::now();
    let mut n = 0;
    while start.elapsed().as_secs() < 8 {
        let Ok(f) = r.frames.recv_timeout(std::time::Duration::from_secs(3)) else { break };
        n += 1;
        if n % 20 != 0 {
            continue;
        }
        let (mut peak, mut idx) = (f32::MIN, 0);
        for (i, &v) in f.db.iter().enumerate() {
            if v > peak {
                peak = v;
                idx = i;
            }
        }
        let mut s: Vec<f32> = f.db.iter().copied().filter(|x| x.is_finite()).collect();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let median = s[s.len() / 2];
        let hz = f.center - f.rate / 2.0 + idx as f64 * f.rate / f.db.len() as f64;
        println!(
            "frame {n:3}  peak {peak:6.1} dBFS at {:9.4} MHz  floor {median:6.1}  peak-floor {:5.1} dB",
            hz / 1e6,
            peak - median
        );
    }
    println!("\n{n} frames in {:.1}s", start.elapsed().as_secs_f64());
    let err = r.status.error.lock().clone();
    if let Some(e) = err {
        println!("error: {e}");
    }
}

fn main() -> eframe::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    if let Some(i) = a.iter().position(|x| x == "--probe") {
        probe(a.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(95.8));
        return Ok(());
    }
    let shot = a
        .iter()
        .position(|x| x == "--shot")
        .map(|i| a.get(i + 1).cloned().unwrap_or_else(|| "/tmp/shot.png".into()));
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(if shot.is_some() { [1400.0, 860.0] } else { [1280.0, 800.0] })
            .with_min_inner_size([800.0, 500.0])
            .with_title("super-radio"),
        ..Default::default()
    };
    eframe::run_native(
        "super-radio",
        opts,
        Box::new(move |cc| {
            let mut app = ui::App::new(cc);
            app.shot = shot;
            Ok(Box::new(app))
        }),
    )
}

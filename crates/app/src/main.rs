mod prof;
mod wheel;
mod bands;
mod dial;
mod theme;
mod radio;
mod ui;
mod waterfall;

/// `--probe <mhz>` runs the radio thread without a window and reports what the
/// waterfall would be drawing, so the signal path can be checked over ssh.
fn probe(mhz: f64, listen: bool) {
    use common::{Hz, Sps};
    let rate = 2_304_000.0;
    let r = radio::Radio::start(0, Hz((mhz * 1e6) as u64), Sps(rate as u64), 2048, || {});
    if listen {
        // Decode a channel off-centre, the case that was dropping samples.
        r.send(radio::Cmd::Demod(radio::Demod::Wfm));
        r.send(radio::Cmd::Listen(Some(0.0)));
        r.send(radio::Cmd::Volume(0.0));
        println!("decoding a WFM channel while measuring");
    }
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
    let dropped = r.status.dropped.load(std::sync::atomic::Ordering::Relaxed);
    println!(
        "\n{n} frames in {:.1}s   dropped {dropped}",
        start.elapsed().as_secs_f64()
    );
    let err = r.status.error.lock().clone();
    if let Some(e) = err {
        println!("error: {e}");
    }
}

/// Time the per-channel audio chain against real time, which is the only
/// number that decides whether the radio thread can keep draining USB.
fn bench_audio() {
    use common::C32;
    let rate = 2_304_000.0;
    let block = 262_144usize;
    let sig: Vec<C32> = (0..block)
        .map(|i| {
            let p = std::f64::consts::TAU * 0.1 * i as f64;
            C32::new(p.cos() as f32 * 0.5, p.sin() as f32 * 0.5)
        })
        .collect();
    println!("{:5} {:>44} {:>10} {:>10}", "mode", "filters", "x real", "us/block");
    for mode in [radio::Demod::Wfm, radio::Demod::Nfm, radio::Demod::Am] {
        let mut a = radio::Audio::new(120_000.0, rate, mode, 48_000.0);
        a.process(&sig, 0.5);
        let reps = 24;
        let t = std::time::Instant::now();
        for _ in 0..reps {
            a.process(&sig, 0.5);
        }
        let el = t.elapsed().as_secs_f64();
        let audio_secs = reps as f64 * block as f64 / rate;
        println!(
            "{:5} {:>44} {:>9.1}x {:>9.0}",
            mode.label(),
            a.cost(),
            audio_secs / el,
            el / reps as f64 * 1e6
        );
    }
}

fn soak_enabled(a: &[String]) -> bool {
    a.iter().any(|x| x == "--soak")
}

fn main() -> eframe::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    if a.iter().any(|x| x == "--bench-audio") {
        bench_audio();
        return Ok(());
    }
    if let Some(i) = a.iter().position(|x| x == "--probe") {
        probe(
            a.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(95.8),
            a.iter().any(|x| x == "--listen"),
        );
        return Ok(());
    }
    let shot = a
        .iter()
        .position(|x| x == "--shot")
        .map(|i| a.get(i + 1).cloned().unwrap_or_else(|| "/tmp/shot.png".into()));
    if soak_enabled(&a) {
        use tracing_subscriber::prelude::*;
        prof::enable();
        tracing_subscriber::registry().with(prof::Timing).init();
    }
    let soak = a
        .iter()
        .position(|x| x == "--soak")
        .map(|i| a.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(12.0f32));
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
            app.soak = soak;
            Ok(Box::new(app))
        }),
    )
}

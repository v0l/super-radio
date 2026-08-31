mod prof;
mod wheel;
mod bands;
mod devices;
mod dial;
mod chainview;
mod theme;
mod radio;
mod record;
mod ui;
mod waterfall;

/// `--probe <mhz>` runs the radio thread without a window and reports what the
/// waterfall would be drawing, so the signal path can be checked over ssh.
fn probe(mhz: f64, listen: bool) {
    use common::{Hz, Sps};
    let rate = 2_304_000.0;
    // Pick by name so the HackRF can be probed while an RTL-SDR is plugged in.
    let want = std::env::args().position(|x| x == "--device").and_then(|i| {
        std::env::args().nth(i + 1)
    });
    let all = devices::list();
    let Some(entry) = want
        .and_then(|w| {
            all.iter()
                .find(|d| d.label.to_lowercase().contains(&w.to_lowercase()))
                .cloned()
        })
        .or_else(|| all.into_iter().next())
    else {
        println!("no radio found");
        return;
    };
    println!("using {}", entry.label);
    let r = radio::Radio::start(entry, Hz((mhz * 1e6) as u64), Sps(rate as u64), 2048, || {});
    // --no-dc leaves the centre spur in, for measuring what removing it does.
    let dc_on = !std::env::args().any(|x| x == "--no-dc");
    r.send(radio::Cmd::DcBlock(dc_on));
    println!("dc block: {}", if dc_on { "on" } else { "off" });
    if listen {
        // Decode a channel off-centre, the case that was dropping samples.
        r.send(radio::Cmd::Demod(radio::Demod::Wfm));
        r.send(radio::Cmd::Listen(Some(0.0)));
        r.send(radio::Cmd::Volume(0.0));
        println!("decoding a WFM channel while measuring");
    }
    let start = std::time::Instant::now();
    let mut n = 0;
    // RDS needs longer than a spectrum check: a station name is four groups
    // and radiotext is sixteen, repeated every couple of seconds.
    let secs = if listen { 25 } else { 8 };
    while start.elapsed().as_secs() < secs {
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
        // The centre bin is where a direct-conversion receiver puts its own
        // leakage, so report it against the floor: that number is the spur.
        let mid = f.db[f.db.len() / 2];
        let hz = f.center - f.rate / 2.0 + idx as f64 * f.rate / f.db.len() as f64;
        println!(
            "frame {n:3}  peak {peak:6.1} dBFS at {:9.4} MHz  floor {median:6.1}  \
             peak-floor {:5.1} dB  centre-floor {:5.1} dB",
            hz / 1e6,
            peak - median,
            mid - median
        );
    }
    if listen {
        let st = r.status.station();
        println!(
            "\nstereo blend {:.2}   PI {}   name {:?}   pty {:?}",
            r.status.blend(),
            st.pi.map(|p| format!("{p:04X}")).unwrap_or_else(|| "-".into()),
            st.name,
            st.pty
        );
        println!(
            "rds groups {}   block errors {}   synced {}",
            st.groups, st.block_errors, st.synced
        );
        if let Some(rt) = st.radiotext {
            println!("radiotext: {rt}");
        }
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

/// Report what is actually in the multiplex at a given station.
///
/// Guessing at why RDS will not decode is expensive: the pilot, the difference
/// subcarrier and RDS are all at known frequencies, so measuring their levels
/// says immediately whether the problem is the receiver or the transmitter.
fn mpx_report(mhz: f64) {
    use common::{Hz, Sps};
    use dsp::rds::RdsDemod;
    use dsp::{FirDecim, FmDemod, StereoDecoder};
    use std::f64::consts::TAU;

    let rate = 2_304_000.0;
    let Some(entry) = devices::list().into_iter().next() else {
        println!("no radio found");
        return;
    };
    let mut dev = match devices::open(&entry) {
        Ok(d) => d,
        Err(e) => {
            println!("open failed: {e}");
            return;
        }
    };
    dev.set_rate(Sps(rate as u64)).ok();
    dev.set_center(Hz((mhz * 1e6) as u64)).ok();
    dev.set_gain("tuner", common::device::GainMode::Auto).ok();
    let mut stream = match dev.start_rx() {
        Ok(s) => s,
        Err(e) => {
            println!("start failed: {e}");
            return;
        }
    };

    let dec = 7usize;
    let if_rate = rate / dec as f64;
    let mut iff = FirDecim::design_hz(rate, dec, 132_000.0, 70.0);
    let mut fm = FmDemod::new(if_rate, 75_000.0);
    let mut st = StereoDecoder::new(if_rate);
    let mut rds = RdsDemod::new(if_rate);
    let (mut iq, mut disc) = (Vec::new(), Vec::new());
    let (mut l, mut r, mut bits) = (Vec::new(), Vec::new(), Vec::new());

    let g = |x: &[f32], f: f64| {
        let k = TAU * f / if_rate;
        let c = 2.0 * k.cos();
        let (mut a, mut b) = (0.0f64, 0.0f64);
        for &v in x {
            let t = v as f64 + c * a - b;
            b = a;
            a = t;
        }
        (a * a + b * b - c * a * b).sqrt() / x.len() as f64
    };

    let start = std::time::Instant::now();
    let mut n = 0;
    while start.elapsed().as_secs() < 12 {
        let Ok(buf) = stream.read() else { break };
        iq.clear();
        iff.process(&buf.samples, &mut iq);
        disc.clear();
        fm.process(&iq, &mut disc);
        st.process(&disc, &mut l, &mut r);
        bits.clear();
        rds.process(&disc, st.phases(), &mut bits);
        n += 1;
        if n % 12 != 0 {
            continue;
        }
        let db = |v: f64, r: f64| 20.0 * (v / r.max(1e-15)).log10();
        let a1 = g(&disc, 1_000.0);
        println!(
            "pilot19 {:6.1} dB   diff38 {:6.1} dB   rds57 {:6.1} dB   (ref audio 1 kHz)                lock {:.2} blend {:.2} | rds level {:.5} arm {} margin {:.2} locked {}",
            db(g(&disc, 19_000.0), a1),
            db(g(&disc, 38_000.0), a1),
            db(g(&disc, 57_000.0), a1),
            st.lock(),
            st.blend(),
            rds.level(),
            rds.timing().0,
            rds.timing().1,
            rds.timing_locked(),
        );
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

/// How long a retune actually costs, which decides whether a drag can send one
/// per frame.
fn bench_tune() {
    use common::Hz;
    let Some(entry) = devices::list().into_iter().next() else {
        println!("no radio found");
        return;
    };
    println!("using {}", entry.label);
    let mut dev = match devices::open(&entry) {
        Ok(d) => d,
        Err(e) => {
            println!("open failed: {e}");
            return;
        }
    };
    let mut best = f64::MAX;
    let mut worst: f64 = 0.0;
    let mut total = 0.0;
    const N: usize = 60;
    for i in 0..N {
        let f = 95_000_000 + (i as u64 % 20) * 25_000;
        let t = std::time::Instant::now();
        let _ = dev.set_center(Hz(f));
        let ms = t.elapsed().as_secs_f64() * 1e3;
        best = best.min(ms);
        worst = worst.max(ms);
        total += ms;
    }
    println!(
        "set_center x{N}:  min {best:.2} ms   mean {:.2} ms   max {worst:.2} ms",
        total / N as f64
    );
    println!("a 60 fps drag spends {:.0}% of each frame retuning", total / N as f64 / 16.7 * 100.0);
}

/// Frames still delivered while the centre is being dragged.
///
/// A drag issues one retune per displayed frame, and a retune blocks the thread
/// that reads samples, so without coalescing the spectrum stops updating for as
/// long as the drag lasts.
fn bench_pan() {
    use common::{Hz, Sps};
    let Some(entry) = devices::list().into_iter().next() else {
        println!("no radio found");
        return;
    };
    println!("using {}", entry.label);
    let r = radio::Radio::start(entry, Hz(95_800_000), Sps(2_304_000), 2048, || {});

    let count = |label: &str, drag: bool| {
        // Settle, then count for three seconds.
        std::thread::sleep(std::time::Duration::from_millis(600));
        while r.frames.try_recv().is_ok() {}
        let t = std::time::Instant::now();
        let mut n = 0;
        let mut step = 0u64;
        while t.elapsed() < std::time::Duration::from_secs(3) {
            if drag {
                // One per frame at 60 fps, which is what a drag produces.
                step = (step + 1) % 200;
                r.send(radio::Cmd::Center(Hz(95_800_000 + step * 500)));
            }
            while r.frames.try_recv().is_ok() {
                n += 1;
            }
            std::thread::sleep(std::time::Duration::from_millis(16));
        }
        println!("{label:22}  {:.1} frames/s", n as f64 / 3.0);
    };
    count("idle", false);
    count(
        if std::env::var("SR_TUNE_GAP_MS").as_deref() == Ok("0") {
            "dragging, uncoalesced"
        } else {
            "dragging the centre"
        },
        true,
    );
    r.send(radio::Cmd::Stop);
    std::thread::sleep(std::time::Duration::from_millis(300));
}

/// Decode a capture, or a directory of them, and print what came out.
///
/// This is the short loop: record once, then run this after every change to
/// a slicer or a protocol and see immediately whether the same burst now
/// decodes. No radio, no waiting for a device to transmit, and the same
/// answer every time.
fn replay(path: &str) -> anyhow::Result<()> {
    let path = std::path::Path::new(path);
    let mut files: Vec<std::path::PathBuf> = if path.is_dir() {
        std::fs::read_dir(path)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                matches!(
                    p.extension().and_then(|s| s.to_str()),
                    Some("cu8" | "cs8" | "cs16" | "cf32" | "data")
                )
            })
            .collect()
    } else {
        vec![path.to_path_buf()]
    };
    files.sort();
    if files.is_empty() {
        anyhow::bail!("no captures in {}", path.display());
    }

    let (mut decoded, mut unknown) = (0, 0);
    for f in &files {
        let name = f.file_name().and_then(|s| s.to_str()).unwrap_or("");
        match radio::replay(f) {
            Ok(recs) if recs.is_empty() => println!("{name}: nothing decoded"),
            Ok(recs) => {
                for r in &recs {
                    if r.model == "unknown" {
                        unknown += 1;
                    } else {
                        decoded += 1;
                    }
                    println!(
                        "{name}: {:.4} MHz {} {:>6.1} dBFS {:>5.1} dB  {:<22} {:>3} B  {}",
                        r.freq / 1e6,
                        r.modulation,
                        r.rssi_dbfs,
                        r.snr_db,
                        r.model,
                        r.bytes.len(),
                        r.detail,
                    );
                }
            }
            Err(e) => println!("{name}: {e}"),
        }
    }
    println!(
        "\n{} capture(s): {decoded} decoded, {unknown} unknown",
        files.len()
    );
    Ok(())
}

fn soak_enabled(a: &[String]) -> bool {
    a.iter().any(|x| x == "--soak")
}

fn main() -> eframe::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    if a.iter().any(|x| x == "--bench-pan") {
        bench_pan();
        return Ok(());
    }
    if a.iter().any(|x| x == "--bench-tune") {
        bench_tune();
        return Ok(());
    }
    if a.iter().any(|x| x == "--bench-audio") {
        bench_audio();
        return Ok(());
    }
    if let Some(i) = a.iter().position(|x| x == "--mpx") {
        mpx_report(a.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(95.8));
        return Ok(());
    }
    if let Some(i) = a.iter().position(|x| x == "--probe") {
        probe(
            a.get(i + 1).and_then(|v| v.parse().ok()).unwrap_or(95.8),
            a.iter().any(|x| x == "--listen"),
        );
        return Ok(());
    }
    if let Some(i) = a.iter().position(|x| x == "--replay") {
        if let Err(e) = replay(a.get(i + 1).map(String::as_str).unwrap_or("captures")) {
            eprintln!("replay failed: {e}");
            std::process::exit(1);
        }
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
    let record = a
        .iter()
        .position(|x| x == "--record")
        .map(|i| a.get(i + 1).cloned().unwrap_or_else(|| "captures".into()))
        .map(std::path::PathBuf::from);
    let record_mb = a
        .iter()
        .position(|x| x == "--record-mb")
        .and_then(|i| a.get(i + 1))
        .and_then(|v| v.parse::<u64>().ok());
    let tune = a
        .iter()
        .position(|x| x == "--tune")
        .and_then(|i| a.get(i + 1))
        .and_then(|v| v.parse::<f64>().ok());
    let mode = a
        .iter()
        .position(|x| x == "--mode")
        .and_then(|i| a.get(i + 1))
        .map(|v| match v.to_ascii_lowercase().as_str() {
            "nfm" | "fm" => radio::Demod::Nfm,
            "am" => radio::Demod::Am,
            "usb" => radio::Demod::Usb,
            "lsb" => radio::Demod::Lsb,
            "cw" => radio::Demod::Cw,
            _ => radio::Demod::Wfm,
        })
        .unwrap_or(radio::Demod::Wfm);
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
            if let Some(mhz) = tune {
                app.tune_to(mhz, mode);
            }
            app.shot = shot;
            if let Some(dir) = record.clone() {
                app.record_to(dir, record_mb);
            }
            if a.iter().any(|x| x == "--gain") {
                app.show_radio_settings();
            }
            if a.iter().any(|x| x == "--chain") {
                app.show_chain();
            }
            app.soak = soak;
            Ok(Box::new(app))
        }),
    )
}

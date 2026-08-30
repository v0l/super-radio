//! HackRF One support, adapting `rs-hackrf` to the workspace `Device` trait.
//!
//! The USB protocol work is `rs-hackrf`'s; this crate is the translation layer:
//! capability reporting, Cs8 to complex float conversion, and the gain policy.

pub mod gain;

use common::{
    Device, DeviceInfo, DriverKind, Error, GainMode, Hz, IqBuf, Result, RxStream, SampleFormat,
    Sps, TunerRange, C32,
};
use rs_hackrf::{AsyncReadControlHandle, AsyncReadHandle, HackRf};

/// Datasheet tuning range.
const FREQ_MIN: u64 = 1_000_000;
const FREQ_MAX: u64 = 6_000_000_000;
/// Below 2 MS/s the USB stream starves; above 20 the host usually cannot keep up.
const RATE_MIN: u64 = 2_000_000;
const RATE_MAX: u64 = 20_000_000;

/// The analogue filter is set to three quarters of the sample rate, so the
/// outer eighth at each edge is roll-off rather than usable span.
const USABLE_RATIO: f32 = 0.75;

fn map_err(e: rs_hackrf::Error) -> Error {
    let s = e.to_string();
    if s.contains("Access") || s.contains("permission") || s.contains("Permission") {
        Error::Permission
    } else if s.contains("busy") || s.contains("Busy") {
        Error::Busy
    } else if s.contains("No HackRF") || s.contains("not found") {
        Error::NoDevice
    } else {
        Error::other(s)
    }
}

/// Serial numbers of every attached HackRF.
pub fn enumerate() -> Vec<String> {
    HackRf::list_devices().unwrap_or_default()
}

pub struct HackRfDevice {
    dev: Option<HackRf>,
    info: DeviceInfo,
    center: Hz,
    rate: Sps,
    gain_db: f32,
    /// Set while streaming so tuning still works without stopping RX.
    ctrl: Option<AsyncReadControlHandle>,
}

impl HackRfDevice {
    pub fn open(index: usize) -> Result<Self> {
        let dev = HackRf::open_by_index(index).map_err(map_err)?;
        let serial = dev
            .board_partid_serialno()
            .map(|(_, _, s)| s)
            .unwrap_or_else(|_| format!("index{index}"));
        let version = dev.version().unwrap_or_else(|_| "unknown".into());
        let board = dev
            .board_id()
            .map(rs_hackrf::transport::board_id_name)
            .unwrap_or("HackRF");

        let info = DeviceInfo {
            kind: DriverKind::HackRf,
            id: serial.clone(),
            label: format!("{board} {}", short_serial(&serial)),
            tuner: format!("MAX2837 / RFFC5072 (fw {version})"),
            ranges: vec![TunerRange {
                range: Hz(FREQ_MIN)..=Hz(FREQ_MAX),
                label: "1 MHz - 6 GHz",
            }],
            rates: Vec::new(),
            rate_range: Sps(RATE_MIN)..=Sps(RATE_MAX),
            // One control, distributed across the three stages. Exposing all
            // three separately is more honest but makes every caller implement
            // the same policy.
            gain_stages: vec![("tuner".into(), 0.0..=gain::MAX_DB)],
            native_format: SampleFormat::Cs8,
            usable_bandwidth_ratio: USABLE_RATIO,
        };

        let mut d = Self {
            dev: Some(dev),
            info,
            center: Hz(100_000_000),
            rate: Sps(8_000_000),
            gain_db: 32.0,
            ctrl: None,
        };
        d.set_rate(Sps(8_000_000))?;
        d.set_center(Hz(100_000_000))?;
        d.apply_gain()?;
        Ok(d)
    }

    pub fn open_first() -> Result<Self> {
        Self::open(0)
    }

    fn hw(&self) -> Result<&HackRf> {
        self.dev.as_ref().ok_or(Error::Disconnected)
    }

    fn apply_gain(&self) -> Result<()> {
        let (amp, lna, vga) = gain::distribute(self.gain_db);
        if let Some(c) = &self.ctrl {
            c.set_amp_enable(amp).map_err(map_err)?;
            c.set_lna_gain(lna).map_err(map_err)?;
            c.set_vga_gain(vga).map_err(map_err)?;
        } else {
            let d = self.hw()?;
            d.set_amp_enable(amp).map_err(map_err)?;
            d.set_lna_gain(lna).map_err(map_err)?;
            d.set_vga_gain(vga).map_err(map_err)?;
        }
        Ok(())
    }
}

fn short_serial(s: &str) -> String {
    // Serials are 32 hex digits and mostly leading zeros; the tail identifies
    // the unit and is what is printed on comparison tools.
    let t = s.trim_start_matches('0');
    if t.len() > 8 { t[t.len() - 8..].to_string() } else { t.to_string() }
}

impl Device for HackRfDevice {
    fn info(&self) -> &DeviceInfo {
        &self.info
    }

    fn set_center(&mut self, f: Hz) -> Result<()> {
        if f.0 < FREQ_MIN || f.0 > FREQ_MAX {
            return Err(Error::FreqOutOfRange { req: f, lo: Hz(FREQ_MIN), hi: Hz(FREQ_MAX) });
        }
        // Retuning through the control handle keeps the stream running; going
        // via the device while streaming would need RX stopped and restarted.
        match &self.ctrl {
            Some(c) => c.tune(f.0).map_err(map_err)?,
            None => self.hw()?.set_freq(f.0).map_err(map_err)?,
        }
        self.center = f;
        Ok(())
    }

    fn center(&self) -> Hz {
        self.center
    }

    fn set_rate(&mut self, r: Sps) -> Result<()> {
        if r.0 < RATE_MIN || r.0 > RATE_MAX {
            return Err(Error::RateUnsupported { req: r });
        }
        let d = self.hw()?;
        d.set_sample_rate(r.0 as u32).map_err(map_err)?;
        // set_sample_rate picks a filter, but be explicit: the default must
        // stay below the rate or out-of-band energy folds into the span.
        let bw = rs_hackrf::transport::compute_baseband_filter_bw((r.0 as u32) * 3 / 4);
        d.set_baseband_filter_bandwidth(bw).map_err(map_err)?;
        self.rate = r;
        Ok(())
    }

    fn rate(&self) -> Sps {
        self.rate
    }

    fn set_gain(&mut self, stage: &str, mode: GainMode) -> Result<()> {
        // No AGC in this hardware, so "auto" is a sensible fixed operating
        // point rather than a silent no-op.
        self.gain_db = match mode {
            GainMode::Auto => 32.0,
            GainMode::Manual(db) => db,
        };
        match stage {
            "tuner" | "" => self.apply_gain(),
            "lna" => {
                let v = gain::quantise_lna(self.gain_db);
                match &self.ctrl {
                    Some(c) => c.set_lna_gain(v).map_err(map_err),
                    None => self.hw()?.set_lna_gain(v).map_err(map_err),
                }
            }
            "vga" => {
                let v = gain::quantise_vga(self.gain_db);
                match &self.ctrl {
                    Some(c) => c.set_vga_gain(v).map_err(map_err),
                    None => self.hw()?.set_vga_gain(v).map_err(map_err),
                }
            }
            "amp" => {
                let on = self.gain_db > 0.0;
                match &self.ctrl {
                    Some(c) => c.set_amp_enable(on).map_err(map_err),
                    None => self.hw()?.set_amp_enable(on).map_err(map_err),
                }
            }
            other => Err(Error::other(format!("no gain stage named {other}"))),
        }
    }

    fn start_rx(&mut self) -> Result<Box<dyn RxStream>> {
        let dev = self.dev.take().ok_or(Error::Disconnected)?;
        let handle = dev.into_streaming_reader(0, 0).map_err(map_err)?;
        self.ctrl = Some(handle.control_handle());
        // Gain is set on the device before streaming, but the control handle
        // is the only route once the device has been consumed.
        let _ = self.apply_gain();
        Ok(Box::new(HackRfStream {
            handle,
            center: self.center,
            rate: self.rate,
            seq: 0,
            last_dropped: 0,
            samples: Vec::new(),
        }))
    }
}

pub struct HackRfStream {
    handle: AsyncReadHandle,
    center: Hz,
    rate: Sps,
    seq: u64,
    last_dropped: u64,
    samples: Vec<C32>,
}

/// Signed 8-bit two's complement to unit-scaled complex float.
fn decode(bytes: &[u8], out: &mut Vec<C32>) {
    out.clear();
    out.reserve(bytes.len() / 2);
    for p in bytes.chunks_exact(2) {
        out.push(C32::new(
            p[0] as i8 as f32 * (1.0 / 128.0),
            p[1] as i8 as f32 * (1.0 / 128.0),
        ));
    }
}

impl RxStream for HackRfStream {
    fn read(&mut self) -> Result<IqBuf> {
        let chunk = match self.handle.recv() {
            Some(Ok(c)) => c,
            Some(Err(e)) => return Err(map_err(e)),
            None => return Err(Error::Disconnected),
        };
        decode(&chunk, &mut self.samples);
        let n = self.samples.len() as u64;
        let buf = IqBuf::new(std::mem::take(&mut self.samples), self.center, self.rate, self.seq);
        self.seq += n;
        Ok(buf)
    }

    fn dropped(&self) -> u64 {
        // Chunks, not samples, so scale by what a chunk holds.
        let c = self.handle.control_handle().dropped_chunks();
        c.saturating_mul((rs_hackrf::TRANSFER_BUFFER_SIZE / 2) as u64)
    }

    fn stop(&mut self) {
        self.handle.stop();
    }
}

impl HackRfStream {
    /// Chunks dropped by the driver since the stream started.
    pub fn dropped_chunks(&self) -> u64 {
        let c = self.handle.control_handle().dropped_chunks();
        self.last_dropped.max(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_bytes_decode_to_the_right_sign_and_scale() {
        let mut out = Vec::new();
        // 0x7f is +127, 0x81 is -127, 0x00 is zero.
        decode(&[0x7f, 0x81, 0x00, 0x40], &mut out);
        assert_eq!(out.len(), 2);
        assert!((out[0].re - 127.0 / 128.0).abs() < 1e-6);
        assert!((out[0].im + 127.0 / 128.0).abs() < 1e-6);
        assert_eq!(out[1].re, 0.0);
        assert!((out[1].im - 0.5).abs() < 1e-6);
    }

    #[test]
    fn decoded_samples_stay_inside_the_unit_circle_ish() {
        let mut out = Vec::new();
        let bytes: Vec<u8> = (0..=255u8).flat_map(|b| [b, b]).collect();
        decode(&bytes, &mut out);
        for c in &out {
            assert!(c.re.abs() <= 1.0 && c.im.abs() <= 1.0, "{c} outside unit scale");
        }
    }

    #[test]
    fn an_odd_trailing_byte_is_ignored_rather_than_panicking() {
        let mut out = Vec::new();
        decode(&[1, 2, 3], &mut out);
        assert_eq!(out.len(), 1, "a half sample must not be invented");
    }

    #[test]
    fn decode_reuses_the_buffer() {
        let mut out = Vec::with_capacity(1024);
        let cap = out.capacity();
        for _ in 0..10 {
            decode(&[1, 2, 3, 4], &mut out);
        }
        assert_eq!(out.len(), 2);
        assert_eq!(out.capacity(), cap, "buffer was reallocated every call");
    }

    #[test]
    fn serials_shorten_to_the_part_that_identifies_the_unit() {
        assert_eq!(short_serial("0000000000000000457863dc3579c1df"), "3579c1df");
        assert_eq!(short_serial("abc"), "abc");
    }
}

//! Oregon Scientific v3 sensors: THGR810 and its rebrands, THN802.
//!
//! 433.92 MHz, Manchester at 1024 baud, and the first user of the Manchester
//! slicer, which until now had no protocol behind it.
//!
//! Three things make this family awkward, all of them from the wire format
//! rather than from the radio:
//!
//! - The Manchester convention is not fixed. Which half of a symbol carries
//!   the bit depends on where the slicer started, so the sync word is searched
//!   for in both polarities and the payload inverted if it was the second one.
//! - Nibbles arrive reversed within each byte, so the frame has to be
//!   nibble-reflected before anything in it means what the layout says.
//! - Values are BCD, including the temperature, which is why 21.7 C is
//!   `0x71 0x02` and not `0xd9`.
//!
//! After reflection:
//!
//! ```text
//! SSSS CDDB TTTT TT?? HH?? KKKK
//! ```
//!
//! - `SSSS` 16 bit sensor id, naming the model
//! - `C`    channel, `DD` rolling device id, `B` battery low in bit 2
//! - `TTTT` temperature, BCD tenths, with a sign bit and a hundreds field
//! - `HH`   humidity, BCD percent
//! - `KKKK` sum of every nibble before it, itself stored across two bytes
//!
//! The checksum is eight bits over fifteen nibbles, which is weak, so a match
//! is only accepted where the sensor id is one this decoder knows and the BCD
//! digits are digits.

use crate::bits::BitBuffer;
use crate::protocol::{DecodeError, Protocol, Report};
use crate::slicer::{Coding, Timing};

pub struct OregonV3;

/// Sync word ending the preamble, in the polarity the layout is written in.
const SYNC: [u8; 2] = [0xff, 0xfa];
/// The same sync inverted, which is how it arrives when the slicer locks onto
/// the other half of the symbol.
const SYNC_INV: [u8; 2] = [0x00, 0x05];
/// Bytes of payload a temperature and humidity frame needs.
const MSG_BYTES: usize = 9;
/// Nibble the checksum starts at.
const CHECKSUM_NIBBLE: usize = 15;

/// Known sensor ids. The THGR810 rolled its id several times across rebrands
/// (Newentor, Unni, Liorque), all differing only in the second nibble.
fn model_of(id: u16) -> Option<&'static str> {
    match id {
        0xf824 | 0xf024 | 0xf224 | 0xfa24 | 0xf8b4 => Some("Oregon-THGR810"),
        0xc844 => Some("Oregon-THN802"),
        _ => None,
    }
}

impl Protocol for OregonV3 {
    fn name(&self) -> &'static str {
        "Oregon-v3"
    }

    fn timing(&self) -> Timing {
        Timing {
            coding: Coding::Manchester,
            // Half a symbol at 1024 baud. Pulses run shorter than pauses on
            // these sensors, which the slicer's rounding absorbs.
            short_us: 488,
            long_us: 976,
            sync_us: 0,
            tolerance_us: 0,
            reset_us: 2400,
        }
    }

    fn decode(&self, bits: &BitBuffer) -> Result<Report, DecodeError> {
        let (at, invert) = match (bits.find(&SYNC, 16), bits.find(&SYNC_INV, 16)) {
            (Some(a), _) => (a, false),
            (None, Some(a)) => (a, true),
            (None, None) => return Err(DecodeError::NotThisProtocol),
        };
        let start = at + 16;
        if start + MSG_BYTES * 8 > bits.len() {
            return Err(DecodeError::WrongLength {
                got: bits.len().saturating_sub(start),
                want: MSG_BYTES * 8,
            });
        }
        let frame = bits.slice(start, MSG_BYTES * 8);
        let frame = if invert { frame.inverted() } else { frame };
        let msg: Vec<u8> = frame.as_bytes().iter().map(|b| b.rotate_left(4)).collect();

        let model = model_of(((msg[0] as u16) << 8) | msg[1] as u16)
            .ok_or(DecodeError::NotThisProtocol)?;
        if nibble_sum(&msg) != ((msg[7] & 0x0f) | (msg[8] & 0xf0)) {
            return Err(DecodeError::CrcFailed);
        }
        // Every value in the frame is BCD, so a nibble above nine is a frame
        // that passed an eight bit checksum by luck.
        if [msg[4] & 0x0f, msg[4] >> 4, msg[5] >> 4, msg[6] & 0x0f, msg[6] >> 4]
            .iter()
            .any(|n| *n > 9)
        {
            return Err(DecodeError::Implausible("value is not BCD"));
        }

        let mut temperature = ((msg[5] >> 4) as f64 * 100.0
            + (msg[4] & 0x0f) as f64 * 10.0
            + (msg[4] >> 4) as f64)
            / 10.0
            + (msg[5] & 0x07) as f64 * 100.0;
        if msg[5] & 0x08 != 0 {
            temperature = -temperature;
        }
        if !(-50.0..=70.0).contains(&temperature) {
            return Err(DecodeError::Implausible("temperature out of range"));
        }
        let humidity = (msg[6] & 0x0f) * 10 + (msg[6] >> 4);
        if humidity > 100 {
            return Err(DecodeError::Implausible("humidity above 100%"));
        }

        let mut r = Report::new(model);
        r.crc_valid = Some(true);
        r.raw = msg.clone();
        r = r
            .int("id", ((msg[2] & 0x0f) | (msg[3] & 0xf0)) as i64)
            .int("channel", (msg[2] >> 4) as i64)
            .float("temperature_c", (temperature * 10.0).round() / 10.0)
            .bool("battery_ok", msg[3] >> 2 & 1 == 0);
        // The THN802 has no humidity element and sends zero for it.
        if humidity != 0 {
            r = r.int("humidity_pct", humidity as i64);
        }
        Ok(r)
    }
}

/// Sum of the fifteen nibbles before the checksum, truncated to eight bits.
fn nibble_sum(msg: &[u8]) -> u8 {
    let whole: u16 = msg[..CHECKSUM_NIBBLE / 2]
        .iter()
        .map(|b| (b >> 4) as u16 + (b & 0x0f) as u16)
        .sum();
    ((whole + (msg[CHECKSUM_NIBBLE / 2] >> 4) as u16) & 0xff) as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Value;

    /// Build a frame the way the sensor sends it: preamble, sync, then the
    /// payload with its nibbles reversed.
    fn frame(id: u16, channel: u8, device: u8, temp_c: f64, humidity: u8, low: bool) -> BitBuffer {
        let mut msg = [0u8; MSG_BYTES];
        msg[0] = (id >> 8) as u8;
        msg[1] = id as u8;
        msg[2] = (channel << 4) | (device & 0x0f);
        msg[3] = (device & 0xf0) | if low { 0x04 } else { 0 };

        let t = (temp_c.abs() * 10.0).round() as u32;
        msg[4] = (((t % 10) as u8) << 4) | ((t / 10 % 10) as u8);
        msg[5] = (((t / 100 % 10) as u8) << 4)
            | if temp_c < 0.0 { 0x08 } else { 0 }
            | (t / 1000) as u8;
        msg[6] = ((humidity % 10) << 4) | (humidity / 10);

        let sum = nibble_sum(&msg);
        msg[7] = (msg[7] & 0xf0) | (sum & 0x0f);
        msg[8] = sum & 0xf0;

        let mut out = BitBuffer::new();
        for _ in 0..24 {
            out.push(true);
        }
        // The sync nibble, completing the 0xff 0xfa pattern the decoder looks
        // for: the preamble above supplies the leading ones.
        for bit in [true, false, true, false] {
            out.push(bit);
        }
        for b in msg {
            let wire = b.rotate_left(4);
            for i in 0..8 {
                out.push(wire & (0x80 >> i) != 0);
            }
        }
        out
    }

    #[test]
    fn decodes_a_thgr810_frame() {
        let r = OregonV3.decode(&frame(0xf824, 1, 0x3a, 21.7, 48, false)).unwrap();
        assert_eq!(r.model, "Oregon-THGR810");
        assert_eq!(r.get("channel"), Some(&Value::Int(1)));
        assert_eq!(r.get("id"), Some(&Value::Int(0x3a)));
        assert_eq!(r.get("temperature_c"), Some(&Value::Float(21.7)));
        assert_eq!(r.get("humidity_pct"), Some(&Value::Int(48)));
        assert_eq!(r.get("battery_ok"), Some(&Value::Bool(true)));
        assert_eq!(r.crc_valid, Some(true));
    }

    #[test]
    fn the_other_manchester_polarity_decodes_the_same() {
        // Which half of the symbol carries the bit depends on where the
        // slicer started, and both happen in practice.
        let f = frame(0xf824, 1, 0x3a, 21.7, 48, false);
        let a = OregonV3.decode(&f).unwrap();
        let b = OregonV3.decode(&f.inverted()).unwrap();
        assert_eq!(a.fields, b.fields);
    }

    #[test]
    fn a_frost_reading_carries_its_sign_bit() {
        let r = OregonV3.decode(&frame(0xf824, 2, 0x3a, -6.3, 91, true)).unwrap();
        assert_eq!(r.get("temperature_c"), Some(&Value::Float(-6.3)));
        assert_eq!(r.get("battery_ok"), Some(&Value::Bool(false)));
    }

    #[test]
    fn a_temperature_only_sensor_reports_no_humidity() {
        let r = OregonV3.decode(&frame(0xc844, 1, 0x11, 19.0, 0, false)).unwrap();
        assert_eq!(r.model, "Oregon-THN802");
        assert!(r.get("humidity_pct").is_none());
    }

    #[test]
    fn an_unknown_sensor_id_is_not_claimed() {
        // The checksum is eight bits over fifteen nibbles, so the id table is
        // doing most of the work of not claiming other people's frames.
        assert_eq!(
            OregonV3.decode(&frame(0x1234, 1, 0x3a, 21.7, 48, false)),
            Err(DecodeError::NotThisProtocol)
        );
    }

    #[test]
    fn a_corrupt_frame_fails_its_checksum() {
        let f = frame(0xf824, 1, 0x3a, 21.7, 48, false);
        let mut broken = BitBuffer::new();
        for i in 0..f.len() {
            broken.push(if i == 60 { !f.get(i).unwrap() } else { f.get(i).unwrap() });
        }
        assert_eq!(OregonV3.decode(&broken), Err(DecodeError::CrcFailed));
    }
}

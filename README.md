# super-radio

A wideband SDR receiver in Rust: channelize a wide span once, then detect and
decode every signal in it in parallel.

## Status

Early. The signal path works end to end and is verified against real off-air
RF, but there is no GUI yet.

Proof it works: `crates/decode/tests/fineoffset_capture.rs` decodes a real
recorded 433.92 MHz weather-station transmission and asserts the result matches
`rtl_433` 25.02 field for field, CRC included. The expected values come from a
separate implementation, so agreement is evidence rather than a restatement of
our own assumptions.

## Layout

| crate | what it does |
|---|---|
| `common` | sample buffers, `Device`/`RxStream` traits, `Hz`/`Sps` units |
| `dsp` | polyphase channelizer, FIR design, mixer, FM/AM demod, burst detector, OOK pulse extraction |
| `pipeline` | the flow graph: typed DAG, rate negotiation, stream tags, events |
| `decode` | bit buffers, pulse slicers, protocol registry, device decoders |
| `nodes` | DSP and decoders as graph nodes, the registry, and the wideband channel bank |
| `sources` | file replay with rtl_433-style filename metadata |
| `audio` | cpal playback with a drift-tracking resampler |
| `app` | egui front end: spectrum, waterfall, tuner, channels |
| `rtlsdr-sys` | bindgen FFI to librtlsdr |
| `hackrf` | HackRF One, adapting `rs-hackrf` to the `Device` trait |
| `rtlsdr` | safe driver with an async streaming thread |
| `hackrf` | pure-Rust USB driver (not started) |
| `app` | egui front end (not started) |

Named `common` rather than `core` because a workspace crate called `core`
shadows the Rust sysroot crate.

## Two design decisions worth knowing

**Serial graphs, parallel channels.** A flow graph runs on one thread; rayon
spreads independent per-channel graphs across the pool. This is the opposite of
GNU Radio's thread-per-block, and it is deliberate: at 512 channels of five
nodes, thread-per-block means 2560 threads on 48 cores and the scheduler costs
more than the DSP.

**Chains are data, not code.** Nodes are registered by name with
introspectable parameters, so an ambiguous signal is attacked by
reconfiguring the chain rather than recompiling. A mistuned chain reports what
it discarded and which parameter to change, because silence is the worst
possible output for a tool meant to identify unknown signals.

**A shared pulse front end, following rtl_433.** Almost every ISM device is OOK
or two-level FSK, and both reduce to mark/gap timings. The DSP runs once per
channel; each protocol is then a timing table and a payload parser working on
integers. That is what makes supporting hundreds of protocols affordable.

## Measured throughput

512 channels, each running a full envelope / pulse-detect / protocol-decode
chain, on a 48-core machine. `x real` above 1.0 keeps up with a live radio.

| input rate | 64 ch | 256 ch | 512 ch |
|---|---|---|---|
| 2.4 MS/s (RTL-SDR) | 10.3x | 9.0x | 11.9x |
| 20 MS/s (HackRF) | 4.9x | 5.6x | 6.1x |
| 50 MS/s | 1.93x | 2.54x | 2.96x |
| 100 MS/s | - | 1.27x | 1.54x |
| 200 MS/s | - | 0.65x | 0.84x |

Real-time ceiling is around 150 MS/s. Channel count is nearly free: 8 to 512
channels costs well under half again as much time, which is the polyphase bank
behaving as advertised.

Getting there needed three things, each found by measuring rather than
guessing, and each initially wrong:

- The **burst detector** ran frame by frame on one thread: 200 million channel
  updates per second of input at 50 MS/s. It now processes channel-major lanes
  in parallel. This was by far the largest cost, and it was invisible until a
  benchmark with *no decode chain at all* still ran at 0.72x.
- The **channelizer** ran on one thread at about 50 MS/s. Overlap-save makes
  every frame independent of every other, so it parallelises; 6x faster.
- The **transpose** wants to be blocked, not eliminated. Letting each channel
  gather its own column has a `channels * 8` byte stride and wastes seven
  eighths of the memory bandwidth.

## Building

```sh
sudo apt install librtlsdr-dev     # or rtl-sdr-devel / rtl-sdr
cargo test --workspace
```

## Test fixtures

Recorded IQ lives on nostr.download, not in git; it is near-incompressible and
would bloat history permanently.

```sh
./testdata/fetch.sh
```

Tests that need a fixture skip cleanly when it is missing, so a fresh clone
builds and passes without network access.

## Try it

```sh
# Sweep a band and rank channels by power
cargo run --release -p rtlsdr --example scan -- 868.3 49.6

# Dump pulse trains and timing histograms from live RF
cargo run --release -p rtlsdr --example ism -- 433.92 30

# Same, from a recorded capture
cargo run --release -p sources --example pulses -- testdata/fineoffset_wh1080_433.92M_250k.cu8

# Build a decode chain at runtime; no argument lists the available nodes
cargo run --release -p nodes --example chain -- \
  testdata/fineoffset_wh1080_433.92M_250k.cu8 \
  'envelope | pulse_detect:reset_us=10000,min_pulses=20 | protocol_decode'

# WFM receiver; verifies itself by finding the 19 kHz stereo pilot
cargo run --release -p rtlsdr --example wfm

# Listen live. 2.304 MS/s / 8 / 6 is exactly 48 kHz, so the chain needs no
# resampling; the drift loop only absorbs crystal mismatch between the radio
# and the sound card.
cargo run --release -p rtlsdr --example listen -- 95.8
cargo run --release -p rtlsdr --example listen -- 446.0 nfm
```

## The app

```
cargo run --release -p app
```

The top bar carries only what you change while tuning: frequency, band, span,
gain, and the fault lamps. Everything else lives behind a cog in the corner of
the pane it affects, because spectrum and waterfall settings are unrelated and
a single settings screen makes both harder to find.

Spectrum and waterfall over a live RTL-SDR. Click the pane to place a channel
and start listening, click an existing marker to switch to it, drag to pan the
centre frequency. Mode defaults from the band plan (WFM in the broadcast band,
AM in the airband, otherwise NFM) and can be overridden per channel.

The display auto-scales against the 10th and 99.9th percentiles rather than the
extremes, so one strong carrier does not flatten everything else.

The frequency readout is a per-digit dial: hover a digit and scroll to step
that decade, the way you pick a tuning step on a hardware receiver. The two
accent colours carry meaning rather than decoration -- amber is what you set,
cyan is what the radio hears -- and the waterfall ramps between them.

Mouse wheel over the spectrum scrubs the centre frequency by a twentieth of the
span per notch; over a digit of the readout it steps that decade. Panning
slides the waterfall history sideways rather than discarding it. Each channel
is drawn at the width its demodulator actually accepts, so an NFM and a WFM
channel on the same frequency look as different as they are.

To find where time is going, `--soak` runs the interface for N seconds, then
reports process CPU and a table of span timings:

```
cargo run --release -p app -- --soak 15
```

Note that span times are wall clock, so `rf_read` sitting near 100% means the
radio thread is idle waiting on USB, which is exactly what it should be doing.

To capture a PNG of the running interface:

```
cargo run --release -p app -- --shot /tmp/shot.png
```

To check the signal path without a display:

```
cargo run --release -p app -- --probe 95.8
```

## Running in the browser

Not implemented. [`docs/web.md`](docs/web.md) has the plan: what crosses
unchanged, what has to be rewritten, and why the wideband bank does not make
the trip.

## Reference implementations

Decoders worth reading before writing our own, all MIT and all pure Rust:

| crate | covers |
|---|---|
| `fmradio` | FM with RDS and adaptive resampling |
| `dabradio` | DAB/DAB+, OFDM and AAC |
| `voracious` | VOR / ILS / DME |
| `jet1090` | ADS-B |
| `ship162` | AIS |
| `datalink` | VDL2 and ARINC 629 |

They share the Desperado workspace with `rs-hackrf`, which this repo already
depends on for HackRF USB support.

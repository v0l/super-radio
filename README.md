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
| `rtlsdr-sys` | bindgen FFI to librtlsdr |
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
chain, on a 48-core machine. `x real` above 1.0 means it keeps up with a live
radio at that rate.

| input rate | 64 ch | 256 ch | 512 ch |
|---|---|---|---|
| 2.4 MS/s (RTL-SDR) | 10.3x | 9.0x | 11.9x |
| 20 MS/s (HackRF) | 1.22x | 1.20x | 1.20x |
| 50 MS/s | 0.52x | 0.46x | 0.47x |

Going from 8 to 512 channels costs under 45% more time, which is the polyphase
bank behaving as advertised: channel count is nearly free, and the per-channel
decode work parallelises cleanly.

50 MS/s is not yet real time. The limit is the channelizer, which runs
single-threaded at about 57 MS/s on its own; the per-channel graphs are not the
problem. Fixing it means block-partitioning the channelizer across threads and
SIMD in the polyphase inner loop, neither of which is done.

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
```

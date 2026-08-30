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

**A shared pulse front end, following rtl_433.** Almost every ISM device is OOK
or two-level FSK, and both reduce to mark/gap timings. The DSP runs once per
channel; each protocol is then a timing table and a payload parser working on
integers. That is what makes supporting hundreds of protocols affordable.

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

# WFM receiver; verifies itself by finding the 19 kHz stereo pilot
cargo run --release -p rtlsdr --example wfm
```

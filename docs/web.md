# Running in the browser

A plan, not a status report. Nothing here is implemented.

The goal is a browser build that tunes a real radio over WebUSB and decodes a
handful of channels, not the full wideband bank. The interesting question is
not whether it runs at all, but which parts of the design survive the move.

## What already crosses unchanged

`dsp`, `decode`, `pipeline` and `nodes` are pure computation with no I/O, no
threads of their own and no platform assumptions. They build for
`wasm32-unknown-unknown` as they stand.

egui and eframe target wasm directly, so `app`'s drawing code needs no port.
cpal has a WebAudio backend, so `audio` keeps its shape; the sink's drift loop
matters more in the browser rather than less, since the audio clock is even
further from the radio's than a sound card's is.

## What does not

**The RTL-SDR driver.** `rtlsdr-sys` is bindgen over librtlsdr, which is C
against libusb. Neither exists in a browser. Replacing it means a pure-Rust
RTL2832U driver: demodulator register access and R820T tuner programming over
control transfers, samples over bulk. This is bounded work with prior art, and
worth doing regardless because it removes the only C dependency in the tree.

**The HackRF transport.** `rs-hackrf` is already pure Rust; only `nusb`
underneath it is native. This is the cheaper of the two and the sensible one to
attempt first.

**Threads.** `rayon` appears in `channelizer.rs`, `detect.rs`, `bank.rs` and
`graph.rs`. In wasm that means `wasm-bindgen-rayon`, Web Workers and
`SharedArrayBuffer`, which in turn means serving with COOP and COEP headers. A
browser build cannot be dropped onto a static host without them.

## Channel budget

This is the part that has to give.

Native measurements: 512 channels of full envelope, pulse detect and protocol
decode run at 2.96x real time on 50 MS/s across 48 cores, and WFM with stereo
and RDS runs at 6.4x real time on one core. A browser has a handful of workers,
not 48, and wasm without SIMD128 typically runs 1.5 to 3 times slower than
native. With SIMD128 it lands nearer the low end of that.

So the wideband bank does not cross. Two or three cores at half native speed
is somewhere near 1% of the throughput the 512-channel figures assume, and USB
in a browser will not sustain 50 MS/s anyway.

What should work is a **small fixed bank**: one channel being demodulated for
audio, and a few more running cheap detectors. Broadcast FM with stereo and
RDS is the honest target for a first build, since it exercises the whole chain,
has a signal available to everyone testing it, and needs one channel.

The channelizer itself does not need changing for this. A polyphase bank with
a small channel count is the same code with a smaller FFT, and the per-channel
cost was already shown to be nearly free; it is the *total* sample rate that
has to come down, not the structure.

Suggested first target, to be confirmed by measurement rather than accepted:

| | native today | browser target |
|---|---|---|
| input rate | up to 150 MS/s | 2.048 MS/s |
| channels | 512 | 8 to 16 |
| audio channels | 1 | 1 |
| RDS | yes | yes |

## Where the seam goes

`common::device` is already the right abstraction. A `WebUsbDevice`
implementing that trait sits beside `rtlsdr` and `hackrf`, and nothing in the
graph, the node registry or the UI needs to know which one it got. The device
selector in `crates/app/src/devices.rs` gains a third enumeration path.

The one thing worth changing first, independent of any browser work, is
splitting USB transport from device logic in the existing drivers. Today the
RTL-SDR driver reaches straight into the C library. If register reads and bulk
reads go behind a small trait, the same tuner and demodulator code serves
native and WebUSB, and the browser port stops being a rewrite.

## Order of work

1. Split transport from device logic in `hackrf`, which is already pure Rust.
2. Build `WebUsbDevice` behind `common::device` and get HackRF running in a
   browser tab. This proves the whole path with the least new code.
3. Port the UI and audio, single channel, no rayon: confirm the frame rate and
   the audio drift loop behave before adding threads.
4. Measure. Decide the channel budget from that number rather than this table.
5. Write the pure-Rust RTL2832U and R820T driver, which is the widest reach
   since it is the cheapest hardware.
6. Add `wasm-bindgen-rayon` and a small channel bank, if step 4 says there is
   room for one.

## Open questions

- Sustained bulk throughput in practice. Browser implementations of rtl_sdr
  run at 2.048 MS/s, but that is a claim to reproduce, not a measurement of
  ours.
- Whether cpal's WebAudio backend gives a queue depth the drift loop can steer,
  or whether it needs an AudioWorklet directly.
- Whether COOP and COEP are acceptable for how this would be hosted. If not,
  the build is single-threaded and the channel budget falls further.
- Whether wasm SIMD128 covers the FFT and FIR inner loops well enough to matter.

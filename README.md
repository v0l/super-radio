# super-radio

A wideband SDR receiver in Rust: channelize a wide span once, then detect and
decode every signal in it in parallel.

## Status

Working receiver, narrow coverage. The signal path runs end to end against real
off-air RF, there is an egui front end, and FM broadcast decodes to stereo audio
with RDS. What is missing is protocols: one ISM device family is implemented
where the goal is hundreds.

Proof it works: `crates/decode/tests/fineoffset_capture.rs` decodes a real
recorded 433.92 MHz weather-station transmission and asserts the result matches
`rtl_433` 25.02 field for field, CRC included. The expected values come from a
separate implementation, so agreement is evidence rather than a restatement of
our own assumptions.

## Layout

| crate | what it does |
|---|---|
| `common` | sample buffers, `Device`/`RxStream` traits, `Hz`/`Sps` units |
| `dsp` | polyphase channelizer, FIR design, mixer, FM/AM demod, FM stereo, RDS, DC blocker, burst detector, OOK/ASK/FSK pulse extraction |
| `pipeline` | the flow graph: typed DAG, rate negotiation, stream tags, events |
| `decode` | bit buffers, pulse slicers, unknown-burst analyser, protocol registry, device decoders |
| `nodes` | DSP and decoders as graph nodes, the registry, and the wideband channel bank |
| `sources` | file replay with rtl_433-style filename metadata |
| `audio` | cpal playback with a drift-tracking resampler |
| `app` | egui front end: spectrum, waterfall, tuner, channels, chain view |
| `rtlsdr-sys` | bindgen FFI to librtlsdr |
| `hackrf` | HackRF One, adapting `rs-hackrf` to the `Device` trait |
| `rtlsdr` | safe driver with an async streaming thread |

Named `common` rather than `core` because a workspace crate called `core`
shadows the Rust sysroot crate.

[`docs/views.md`](docs/views.md) describes the packet stream as a bus: the
list and the waterfall marks are views over it, and a map, an image pane or a
chart attach the same way, by reading a packet's structured fields and its
media type rather than the demodulator that produced it.

[`docs/protocols.md`](docs/protocols.md) is the protocol roadmap: everything
rtl_433, a Flipper, a PortaPack and SDRangel can do, with what each one costs
here, which direction is realistic for it, and what the transmit path needs
before any of it can transmit.

## Design decisions worth knowing

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
or two-level FSK, and both reduce to mark/gap timings: `pulse_detect` thresholds
an envelope, `fsk_detect` thresholds a discriminator, `ask_detect` covers keying
too shallow for the first to see, and the slicers and protocols below them
cannot tell which one they were fed. The DSP runs once per
channel; each protocol is then a timing table and a payload parser working on
integers. That is what makes supporting hundreds of protocols affordable.

**The app runs on the graph, not beside it.** The audio chain is a
`pipeline::Graph` built from the same nodes everything else uses, so rate
negotiation and latency accounting happen in one place. The chain view draws
that graph's topology rather than a diagram kept alongside it, which is the only
way it can be trusted: documentation that has drifted is worse than none,
because it is believed.

## Measured throughput

512 channels, each running a full envelope / pulse-detect / protocol-decode
chain, on a 48-thread machine. `x real` above 1.0 keeps up with a live radio.

| input rate | 8 ch | 64 ch | 256 ch | 512 ch |
|---|---|---|---|---|
| 2.4 MS/s (RTL-SDR) | 22.9x | 58.9x | 64.9x | 67.3x |
| 20 MS/s (HackRF) | 3.17x | 7.54x | 9.30x | 10.2x |
| 50 MS/s | 1.34x | 2.69x | 3.63x | 4.19x |
| 100 MS/s | 0.68x | 1.01x | 1.91x | 2.14x |
| 200 MS/s | 0.33x | 0.55x | 0.82x | 1.03x |

Reproduce with `cargo run --release -p nodes --example bench -- 50000000`, in Hz.

Real-time ceiling is a little over 200 MS/s at 512 channels. More channels is
cheaper, not dearer: 8 to 512 channels is three times *faster* at every input
rate, because splitting a fixed input rate across more channels drops the rate
each one runs at, and the filter behind each channel costs per sample. Channel
count is not the thing to economise on. Input rate is.

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

Run the tests in release. Several of them assert the audio chain keeps ahead of
real time, and a debug build misses by enough that the numbers mean nothing.

### Prebuilt binaries

`.github/workflows/build.yml` builds Linux, macOS (Intel and Apple Silicon) and
Windows on every push, and attaches them to a release on a `v*` tag. The
binaries link librtlsdr rather than bundling it, so the copy matching your udev
rules is the one already installed; the Windows zip does ship the DLLs, since
there is no system package to point a Windows user at. Windows also needs
WinUSB bound to the RTL2832U with Zadig before the device can be opened.

## Test fixtures

Recorded IQ lives on nostr.download, not in git; it is near-incompressible and
would bloat history permanently.

```sh
./testdata/fetch.sh
```

Tests that need a fixture skip cleanly when it is missing, so a fresh clone
builds and passes without network access.

### Making your own

`--record <dir>` writes every burst the scanner reports, whether it decoded or
not, as an ordinary rtl_433 style capture:

```sh
super-radio --tune 868.3 --record captures
super-radio --replay captures
```

The recorder keeps the last three quarters of a second of signal in memory and
writes it out when a decode arrives, because a packet is reported long after it
was transmitted: the transmission itself takes tens of milliseconds, the pulse
detector waits for the silence after it, and the filters add their own latency.
Measured on the Fine Offset capture, a quarter of a second of history loses the
packet entirely and three tenths catches it.

Each burst is mixed down to its own frequency, decimated to 250 kS/s and
written as `g0001_<protocol>_<mod>_<freq>M_<rate>k.cu8`, about 380 kB, so the
filename alone tells `sources::FileSource` and rtl_433 everything they need. An
`index.jsonl` alongside records what was made of each one. Recording stops
after 256 MB rather than filling the disk overnight.

This is the short loop for protocol work: capture the band once, then run
`--replay` after every change and see immediately whether the same burst now
decodes, with no radio and the same answer every time. A capture that decodes
is also a test fixture.

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

# Listen live. 2.304 MS/s / 8 / 6 is exactly 48 kHz, so this example needs no
# resampling; the drift loop only absorbs crystal mismatch between the radio
# and the sound card. The app picks its own rates and does resample, because
# WFM needs an IF above 330 kHz to reach the 57 kHz RDS subcarrier and 2.304
# does not divide into that and 48 kHz both.
cargo run --release -p rtlsdr --example listen -- 95.8
cargo run --release -p rtlsdr --example listen -- 446.0 nfm
```

## The app

```
cargo run --release -p app
```

The top bar carries only what you change while tuning: frequency, band,
bandwidth, view, gain, the device with its start and stop, and the fault lamps.
Everything else lives behind a cog in the corner of the pane it affects, because
spectrum and waterfall settings are unrelated and a single settings screen makes
both harder to find.

Stopping releases the USB claim without quitting, which is the only way to hand
the radio to another program: a held claim is what makes the next process fail
to open it at all.

### Spectrum

Click the pane to place a channel and start listening, click an existing marker
to switch to it, drag a marker to move that channel, drag anywhere else to pan.
Which one a drag means is decided when it starts, so the gesture cannot change
meaning halfway through as the pointer leaves the line it grabbed.

Hold shift while placing or dragging to snap to the band's channel plan: 100 kHz
in the FM broadcast band, 25 kHz in the airband and marine VHF, 12.5 kHz on
2 m and 70 cm. Bands with no even raster declare none and do not snap, because
moving a channel off the signal it was aimed at is worse than not helping.

Mode defaults from the band plan (WFM in the broadcast band, AM in the airband,
otherwise NFM) and can be overridden per channel: WFM, NFM and AM for
broadcast and utility listening, USB, LSB and CW for the amateur bands.

The three amateur modes share a sideband filter with complex taps, which is
the direct way to keep one sideband and reject the other: an ordinary lowpass
modulated up to the sideband's centre. A 2.4 kHz voice filter at 48 kHz takes
363 taps, puts its stated edges on -6 dB, and rejects the wrong sideband by
more than 50 dB. CW is the same filter at 500 Hz around the pitch, and the
receiver is tuned low by that pitch so the dial reads the transmitted carrier
rather than the note in your ears.

Both are unusable without gain control, because an SSB signal arrives with no
level control of any kind at the far end. The AGC is the conventional shape,
fast attack and slow release with a hang in between, and it freezes while the
squelch is shut: releasing through a muted channel winds the gain to maximum
and then blasts the first syllable after the squelch opens.

FM squelches on noise rather than level. An FM discriminator with no signal on
it produces mostly high frequency hiss, and any signal at all pushes that down
because FM captures, so measuring the energy above the speech band finds a
station at the point where it becomes intelligible instead of at some level
picked in advance. That measurement has to happen on the discriminator's raw
output: with the squelch after the audio filter, the filter had already removed
the noise the meter looks for and the squelch sat open on an empty channel.
AM, USB, LSB and CW have no capture effect to measure, so they squelch on
level, set low because those modes are usually listened to wide open.

The display auto-scales against the 10th and 99.9th percentiles rather than the
extremes, so one strong carrier does not flatten everything else.

Frequencies are per-digit dials, one for the receiver and one in each channel:
hover a digit and scroll to step that decade, the way you pick a tuning step on
a hardware receiver, and right-click a digit to clear it and everything under
it. Mouse wheel over the spectrum itself scrubs the centre by a twentieth of
the span per notch.

The two accent colours carry meaning rather than decoration: amber is what you
set, cyan is what the radio hears, so an RDS station name is cyan. The waterfall
ramps between them.

Panning slides the waterfall history sideways rather than discarding it, and the
trace is positioned by the frequency its data was taken at, so it slides under a
drag instead of being stretched across the pane while the radio catches up.
Retuning costs about 25 ms of blocked USB, so they are spaced out rather than
issued once per frame.

Each channel is drawn at the width its demodulator actually accepts, so an NFM
and a WFM channel on the same frequency look as different as they are.

The spectrum and the waterfall are split by a handle under the band ribbon:
drag it to give either one more room, double click it to go back to the
default. The packet log resizes the same way, by its top edge. Which pane
matters depends on what is being looked for, so none of the three is fixed.

### Decoding the whole span

Data decoding is not something you tune to. The receiver splits whatever span
it is on into channels and runs a decoder on every one of them at once, all
the time, so a sensor that transmits once a minute is caught whether or not you
were looking at its frequency. Both an OOK and an FSK front end run over the
whole span, because which modulation a device uses is not visible in a
waterfall.

It splits the span twice, because the two front ends want opposite things from
a channel. Measured by adding noise to the Fine Offset capture until decoding
stops, a 1.5 kbit/s OOK sensor survives down to 12.3 dB peak-to-noise in a
31 kHz channel and needs 22.9 dB in a 125 kHz one: a wide channel integrates
noise across its whole width while the signal occupies a sliver of it, so it
costs 10.6 dB for nothing. FSK needs the opposite, because its two tones are
tens of kHz apart and a narrow channel cuts one of them off: the same
synthetic packet reads as 46 bits at 110 us a symbol in a 125 kHz channel and
as eight bits of nonsense in a 31 kHz one. So there are two channelizers, a
31.25 kHz bank feeding the OOK path and a 125 kHz bank feeding the FSK path.

Neither front end needs the signal centred, which is what makes any of it
work: the OOK path is an envelope detector and does not care where in the
channel the carrier sits, and the FSK path measures both tones from the burst
itself, so a SAW transmitter tens of kHz off nominal reads the same as one on
frequency. Channel width therefore costs sensitivity and nothing else.

Channels overlap by design, so one burst is seen by several of them and by
both banks at once, each reading a different mangled copy. The strongest
report of a burst wins and a real decode beats a louder guess, so a
transmission appears once. A device that repeats its packet still gets a row
per repeat, because those are separate bursts on the same channel through the
same front end.

Bursts that match no protocol are reported too, and that is the point of
scanning a band rather than a frequency: an unknown device is exactly what
should be surfaced. The coding is inferred from the burst's own histograms
(two mark widths and one gap width is PWM, one mark and two gaps is PPM,
widths at T and 2T in both is Manchester), the symbol timings are measured
from the burst, and the bits are sliced out under that reading. It is a guess,
labelled as one, and it is enough to recognise the same device ID across
several receptions, which is where reverse engineering starts.

The packet list is the bottom pane:

```
 no     time   frequency   mod   rssi   snr  protocol           len  info
 27    4.267  433.9200MHz  OOK  -18.0  21.5  Fineoffset-WHx080   11  station_id=196 temperature_c=16.2
 28    4.284  868.3500MHz  FSK  -26.0  13.0  unknown              6  PWM 488/1464 us, 46 pulses, 46 bits
```

RSSI and SNR are both there because either alone misleads: a strong packet in
a noisy channel and a weak one in a quiet channel share an SNR, and only the
level says the front end is clipping. It is referenced to full scale at the
detector, not to the antenna, so it compares packets on one receiver and is
not a field strength. Clicking a packet opens an offset/hex/ASCII dump of its
bytes. `UNKNOWN` hides unclaimed bursts when a noisy band buries the decodes.

Each packet is also marked on the waterfall, written into the history itself
rather than painted over it, so it scrolls, pans and ages with the row it
arrived on and cannot drift away from the trace that produced it. The bracket
frames the signal instead of covering it, coloured green for a verified
packet, amber for one with no check to verify, red for a failed one and grey
for an unknown. Beside it is the packet's number from the list, which is all
the text worth putting over a waterfall: a protocol name is unreadable at the
sizes that matter, and the number leads to a row that has room to say more.

Idle channels cost only the burst detector, which is why the whole band can be
covered continuously even with two banks running: measured at 2.4 MS/s over 78
narrow and 20 wide channels, the scanner runs at about 17x real time.
`DECODE ALL` in the top bar turns it off, `LOG` hides the list.

### Signal chain

The other view draws the graph the listening channel is running, with the type
and rate on every link and the delay through the whole chain. It is read from
the built graph, so it shows what is running rather than what was intended.

### FM broadcast

WFM decodes stereo and RDS from one pilot PLL. The station name, programme type
and radiotext appear in the channel card. A block that fails its syndrome check
is discarded rather than published, so a station with no RDS shows nothing at
all instead of plausible noise.

### Direct conversion spur

A zero-IF receiver puts oscillator leakage at exactly the tuned frequency, which
reads as a very strong carrier and is not a signal. It is removed by default;
the toggle is under the spectrum cog. Measured on a HackRF at 95.8 MHz the
centre bin sits 47 dB above the noise floor with this off and 1 dB below it
with this on.

### Command line

```
--tune <mhz>        start tuned and listening
--mode <mode>       wfm, nfm, am, usb, lsb or cw (default wfm)
--chain             open on the signal chain view
--device <name>     pick a radio by name, for when several are plugged in
--shot <path>       write a PNG of the interface and exit
--record <dir>      write every burst that decodes to a directory of captures
--record-mb <n>     how much to write before recording stops (default 256)
--replay <path>     decode a capture, or a directory of them, and print it
--soak <secs>       run for N seconds, then report CPU and span timings
--probe <mhz>       check the signal path with no display
--mpx <mhz>         report FM multiplex levels
--no-dc             leave the centre spur in
--bench-tune        time a retune
--bench-pan         frames delivered while the centre is dragged
--bench-audio       audio chain throughput
```

Span times under `--soak` are wall clock, so `rf_read` sitting near 100% means
the radio thread is idle waiting on USB, which is exactly what it should be
doing.

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

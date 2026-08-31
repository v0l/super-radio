# Protocols

The target is the union of what rtl_433, a Flipper Zero, a PortaPack running
Mayhem and SDRangel can do, in one receiver, with transmit where transmit is
lawful. This file lists those protocols, what each one costs to add, and which
direction is realistic for it.

The point of the list is to make the cost visible before starting, because
"add a protocol" ranges from a twenty line table to a new receiver.

## What decides the cost

Everything downstream of a burst is cheap. The expensive part is the front end
that turns radio into symbols, and there are only a few of those.

| Front end | Produces | Receive | Transmit |
|---|---|---|---|
| envelope, `pulse_detect` | mark/gap timings from OOK | yes | no |
| buffered envelope, `ask_detect` | mark/gap timings from shallow ASK | yes | no |
| discriminator, `fsk_detect` | mark/gap timings from two-level FSK | yes | no |
| pilot PLL, `wfm` | stereo audio, RDS | yes | no |
| four-level slicer | 4-FSK symbols | no | no |
| coherent PSK/GMSK with timing recovery | soft symbols | no | no |
| chirp correlator (dechirp then FFT) | LoRa symbols | no | no |
| OFDM (FFT, pilots, equaliser) | subcarrier symbols | no | no |
| DSSS despreader | chip-synchronised symbols | no | no |

A protocol whose symbols reach the mark/gap layer costs a timing table and a
payload parser, and nothing else: the slicers (PWM, PPM, Manchester, NRZ), the
CRC helpers, the unknown-burst analyser and the packet list already exist.
Everything else costs a demodulator first.

Transmit inverts the same layers and none of it is written yet. See
[Transmit](#transmit) below.

The second constraint is channel width. The scanner runs two channelizers: a
31.25 kHz bank feeding the OOK front end and a 125 kHz bank feeding the FSK
one. Anything wider than about 100 kHz occupied needs a third, wider tier, and
anything past a few hundred kHz needs a chain of its own rather than a channel
in a bank.

The third is hardware.

| Radio | Range | Rate | Direction |
|---|---|---|---|
| RTL-SDR | 24-1766 MHz | 2.4 MS/s | receive only |
| HackRF One | 1 MHz-6 GHz | 20 MS/s | half duplex, transmit and receive |
| PortaPack | a HackRF with a screen | as HackRF | as HackRF |

Out of scope whatever the ambition: a Flipper's 125 kHz RFID, its 13.56 MHz
NFC, its infrared and its iButton are near-field or optical, not radio an SDR
can reach.

## Status codes

Receive:

- **done**: decoding now, verified against a recording
- **table**: fits an existing front end. A timing table plus a payload parser
- **framing**: fits an existing front end, but needs sync words, bit
  destuffing or forward error correction that is not written yet
- **demod**: needs a demodulator this project does not have
- **chain**: needs a receive chain of its own, not a channel in a bank

Transmit:

- **table**: the same timing table, run backwards, once the transmit path
  exists
- **mod**: needs a modulator beyond OOK/FSK keying
- **chain**: needs its own transmit chain
- **no**: technically possible, not lawful to transmit in normal use. Listed
  so the answer is recorded rather than rediscovered

Transmitting is regulated. ISM band transmission is bounded by power and duty
cycle limits, amateur bands need a licence, and aviation, maritime, paging,
public safety and cellular bands are not open to transmit on at all. Cloning
someone else's remote or jamming anything is not a feature this project wants.

## ISM sensors, remotes and telemetry

The rtl_433 and Flipper sub-GHz domain: roughly 250 device decoders in
rtl_433, almost all OOK or two-level FSK, almost all reachable from the
existing pulse front end. Lowest marginal cost, highest coverage gain.

| Protocol | Where | Modulation | Width | RX | TX | Notes |
|---|---|---|---|---|---|---|
| Fine Offset WH1080 family | 433.92 MHz | OOK PWM 544/1524 us | 31 kHz | done | table | CRC8, matches rtl_433 25.02 field for field |
| PT2262 / EV1527 / HS1527 fixed code | 315/433.92 MHz | OOK PWM | 31 kHz | table | table | Garage doors, doorbells, cheap sensors. The most common thing on 433 |
| Nice Flo, CAME, Holtek, Ansonic, Linear | 433.92 MHz | OOK PWM | 31 kHz | table | table | Flipper's fixed-code gate remotes, one table each |
| Chamberlain / Security+ 1.0 and 2.0 | 310/315/390 MHz | OOK PWM | 31 kHz | table | table | Rolling code: readable, not cloneable |
| KeeLoq, FAAC SLH, Somfy RTS, Star Line | 433.42/433.92 MHz | OOK PWM/Manchester | 31 kHz | table | no | Frames read fine; the code is encrypted by design, and transmitting one is cloning someone's gate |
| Acurite, Ambient Weather, LaCrosse, Oregon Scientific | 433.92/915 MHz | OOK PWM/Manchester | 31 kHz | table | table | Several families each, all timing tables |
| TPMS (Schrader, Toyota, Renault, Citroen) | 315/433.92 MHz | OOK/FSK Manchester | 31-125 kHz | table | no | Bursty, short, CRC8. Faking tyre pressure to a moving car is not a feature |
| EnOcean | 868.3 MHz | ASK | 31 kHz | table | table | Self-powered switches |
| Itron / ERT smart meters | 902-928 MHz | OOK/FSK Manchester | 125 kHz | table | no | The rtlamr target |
| X10 RF | 310/433.92 MHz | OOK | 31 kHz | table | table | |
| Homematic | 868.3 MHz | GFSK 10 kbps | 125 kHz | framing | mod | Sync word plus whitening |
| Radiosondes (RS41, DFM, M10) | 400-406 MHz | GFSK 4800 bps | 125 kHz | framing | no | Reed-Solomon, and a GPS position worth having |
| nRF24 ShockBurst | 2.4 GHz | GFSK 1-2 Mbps | 2 MHz | demod | mod | HackRF only. Flipper does this with a separate module |

## Utility metering and home automation

| Protocol | Where | Modulation | Width | RX | TX | Notes |
|---|---|---|---|---|---|---|
| Wireless M-Bus mode T | 868.95 MHz | 2-FSK 100 kbps, 3-of-6 | 125 kHz | framing | no | Very common on 868. Block CRCs, payloads often encrypted |
| Wireless M-Bus mode S | 868.3 MHz | 2-FSK 32.768 kbps, Manchester | 125 kHz | framing | no | |
| Wireless M-Bus mode C | 868.95 MHz | 2-FSK 100 kbps NRZ | 125 kHz | framing | no | |
| Wireless M-Bus mode N | 169 MHz | 4-GFSK 2.4/4.8 kbps | 31 kHz | demod | mod | Four levels, so the two-level slicer does not apply |
| Z-Wave R1 | 868.42/908.42 MHz | FSK 9.6 kbps, Manchester | 125 kHz | framing | table | Preamble, sync byte, checksum |
| Z-Wave R2/R3 | 868.42/908.42 MHz | FSK 40/100 kbps | 125 kHz | framing | table | |
| Zigbee / 802.15.4 sub-GHz | 868/915 MHz | BPSK DSSS | 125 kHz | demod | mod | Needs a despreader |
| Zigbee / 802.15.4 | 2.4 GHz | O-QPSK DSSS 250 kbps | 2 MHz | demod | mod | HackRF only |
| Bluetooth LE advertising | 2.4 GHz | GFSK 1 Mbps | 2 MHz | demod | mod | Hopping and whitening. HackRF only |

## LPWAN

| Protocol | Where | Modulation | Width | RX | TX | Notes |
|---|---|---|---|---|---|---|
| LoRa | 433/868/915 MHz | CSS chirp SF7-12 | 125-500 kHz | demod | mod | Dechirp with a conjugate chirp then FFT. Self-contained, well documented, highest value item on the demod list |
| LoRaWAN | as LoRa | as LoRa | 125-500 kHz | demod | no | Payloads are AES encrypted; the metadata is still worth logging |
| Meshtastic | 433/868/915 MHz | LoRa | 250 kHz | demod | mod | LoRa plus a known framing, and lawful to transmit on your own mesh |
| Sigfox uplink | 868.13 MHz | DBPSK 100 bps (600 US) | 100 Hz | demod | mod | Ultra narrowband, coherent detection, very narrow channel |
| Sigfox downlink | 869.525 MHz | GFSK 600 bps | 31 kHz | framing | no | |

## Aviation

| Protocol | Where | Modulation | Width | RX | TX | Notes |
|---|---|---|---|---|---|---|
| ADS-B 1090ES (Mode S) | 1090 MHz | PPM 1 Mbit/s | 2 MHz | chain | no | The pulse layer fits, the rate does not: 0.5 us half-bits need 2 MS/s or better, so it wants its own chain. CRC24. PortaPack transmits this; broadcasting fake aircraft is not something to build |
| Mode A/C | 1090 MHz | pulse pairs | 2 MHz | chain | no | Same chain as Mode S once it exists |
| UAT | 978 MHz | CPFSK 1.041667 Mbps | 2 MHz | chain | no | US general aviation, Reed-Solomon |
| ACARS | 129-137 MHz | AM, MSK 2400 bps | 25 kHz | framing | no | Rides on an AM channel: envelope path plus MSK bit recovery |
| VDL Mode 2 | 136 MHz | D8PSK 31.5 kbps | 25 kHz | demod | no | Differential 8-PSK, so a coherent chain |
| VOR / ILS | 108-118 MHz | AM with 30 Hz subcarriers | 25 kHz | framing | no | SDRangel decodes bearing from these; the maths is small |
| HFDL | 2-22 MHz | PSK | 3 kHz | demod | no | Needs HF hardware too |

## Maritime

| Protocol | Where | Modulation | Width | RX | TX | Notes |
|---|---|---|---|---|---|---|
| AIS | 161.975/162.025 MHz | GMSK 9600 bps | 25 kHz | framing | no | NRZI, HDLC bit stuffing, CRC16. The discriminator output is usable directly, so this is the cheapest of the "real" protocols |
| DSC | 156.525 MHz, HF | FSK 1200 baud | 25 kHz | framing | no | Distress calls. Transmitting is a criminal matter, not a licensing one |
| NAVTEX | 518 kHz | FSK 100 baud SITOR-B | 1 kHz | chain | no | Needs HF hardware |

## Paging

| Protocol | Where | Modulation | Width | RX | TX | Notes |
|---|---|---|---|---|---|---|
| POCSAG | 137-174, 450-470, 929 MHz | 2-FSK 512/1200/2400 bps | 25 kHz | framing | table | Fits the FSK front end directly; needs sync word search and BCH(31,21). Amateur DAPNET networks make transmit lawful for licence holders |
| FLEX | 929-932 MHz | 2/4-FSK 1600-6400 bps | 25 kHz | demod | no | The four-level modes need a four-way slicer |
| ERMES | 169 MHz | 4-FSK 6250 bps | 25 kHz | demod | no | As FLEX |

Message content may be legally protected. Decoding and displaying other
people's messages is regulated differently in different countries; this is a
capability note, not advice.

## Land mobile and digital voice

| Protocol | Where | Modulation | Width | RX | TX | Notes |
|---|---|---|---|---|---|---|
| DMR | 136-174, 400-470 MHz | 4-FSK 4800 baud | 12.5 kHz | demod | no | Four-level slicer, then AMBE, which is patent encumbered |
| P25 phase 1 | 700-900 MHz | C4FM | 12.5 kHz | demod | no | As DMR, plus IMBE |
| NXDN, dPMR | 400-470 MHz | 4-FSK | 6.25/12.5 kHz | demod | no | |
| M17 | amateur bands | 4-FSK 4800 baud | 12.5 kHz | demod | mod | Open codec and open spec, so the only one here worth transmitting, and lawful with a licence |
| TETRA | 380-400, 410-430 MHz | pi/4-DQPSK 36 kbps | 25 kHz | demod | no | Coherent differential PSK |
| FM with CTCSS/DCS | any | FM plus subaudible tone | 12.5 kHz | table | mod | Trivial next to the rest: a Goertzel on the discriminator output |

## Broadcast

| Protocol | Where | Modulation | Width | RX | TX | Notes |
|---|---|---|---|---|---|---|
| FM stereo | 87.5-108 MHz | FM, 38 kHz subcarrier | 200 kHz | done | mod | Transmitting needs a dummy load or a licence |
| RDS | 87.5-108 MHz | 57 kHz BPSK 1187.5 bps | 200 kHz | done | mod | PortaPack transmits RDS; the encoder is small once the modulator exists |
| AM broadcast | 530-1700 kHz | AM | 10 kHz | done | mod | Envelope detector; the band itself needs HF hardware |
| DAB / DAB+ | 174-240 MHz | OFDM DQPSK | 1.536 MHz | chain | no | Viterbi plus Reed-Solomon after the OFDM |
| DVB-T | 470-790 MHz | OFDM | 8 MHz | chain | no | HackRF only, and a large amount of machinery |
| DRM | HF | OFDM | 10 kHz | chain | no | |
| HD Radio (IBOC) | 88-108 MHz | OFDM sidebands | 400 kHz | chain | no | |

## Satellite

| Protocol | Where | Modulation | Width | RX | TX | Notes |
|---|---|---|---|---|---|---|
| NOAA APT | 137 MHz | FM, 2.4 kHz AM subcarrier | 40 kHz | framing | no | An image rather than packets: the demodulation is easy, the presentation is the work |
| Meteor-M LRPT | 137.9 MHz | QPSK 72 kbps | 120 kHz | demod | no | Viterbi plus Reed-Solomon |
| Iridium | 1616-1626 MHz | QPSK 25 kbaud bursts | 500 kHz | demod | no | Bursty, needs good timing |
| Inmarsat STD-C | 1537 MHz | BPSK 1200 bps | 10 kHz | demod | no | Needs an L-band antenna and an LNA |
| GOES HRIT | 1694 MHz | BPSK 927 kbps | 2 MHz | chain | no | |
| GPS L1 | 1575.42 MHz | BPSK DSSS | 2 MHz | demod | mod | Receiving needs a despreader. PortaPack simulates GPS; transmitting is jamming with extra steps |

## Amateur

| Protocol | Where | Modulation | Width | RX | TX | Notes |
|---|---|---|---|---|---|---|
| APRS / AX.25 1200 | 144.39/144.8 MHz | AFSK over FM | 12.5 kHz | framing | mod | Discriminator, Bell 202 tones, HDLC, CRC16. A good first framing target, and lawful to transmit with a licence |
| Packet 9600 (G3RUH) | 144-440 MHz | direct FSK 9600 | 25 kHz | framing | mod | Scrambled NRZI |
| Morse (CW) | any | OOK | 500 Hz | table | table | The envelope path already produces the timings, and keying a carrier is the simplest transmit case there is |
| RTTY | HF, VHF | FSK 45.45 baud | 1 kHz | framing | mod | Baudot, and the same two-tone shape as everything else here |
| PSK31 | HF | BPSK 31.25 baud | 100 Hz | demod | mod | Varicode, coherent |
| SSTV | HF, 144 MHz | FM subcarrier | 3 kHz | framing | mod | Image, like APT |
| FT8, WSPR, JS8 | HF, 6 m | narrow MFSK | 50 Hz-3 kHz | demod | mod | Long coherent integration and LDPC. A different kind of receiver |
| DTMF and tone remote | any | audio tones over FM | 12.5 kHz | table | mod | Goertzel pair |

## Cellular

| Protocol | Where | Modulation | Width | RX | TX | Notes |
|---|---|---|---|---|---|---|
| GSM downlink control | 900/1800 MHz | GMSK 270.833 kbps | 200 kHz | demod | no | Broadcast channels carry cell identity in the clear; traffic uses A5 ciphers and breaking those is illegal in most places. Only the control plane is worth building |
| LTE / 5G | various | OFDM | 1.4-100 MHz | chain | no | Cell search and MIB decode is possible in principle; past that it is a stack, not a decoder |

## Time and beacons

| Protocol | Where | Modulation | Width | RX | TX | Notes |
|---|---|---|---|---|---|---|
| DCF77 | 77.5 kHz | AM plus phase modulation | 100 Hz | chain | no | Needs VLF hardware |
| MSF, WWVB | 60 kHz | AM | 100 Hz | chain | no | As DCF77 |
| NDB beacons | 190-535 kHz | keyed carrier | 1 kHz | chain | no | |

## Transmit

Nothing transmits yet, and the gap is structural rather than protocol by
protocol. Four things are missing:

1. **A transmitting device.** `common::device::Device` has `start_rx` and no
   counterpart. It needs `start_tx` returning a sink, and the `hackrf` crate
   needs the transmit half of the USB protocol and its gain controls. The
   RTL-SDR cannot transmit at all, so the trait has to make that a capability
   rather than an assumption.

2. **Encoders, which are the slicers backwards.** `decode::slicer::slice`
   turns a pulse train into bits under a timing table; the same table turns
   bits back into a pulse train. Every protocol that has a table gets an
   encoder nearly for free, which is why the transmit column above mostly
   mirrors the receive one.

3. **Modulator nodes.** A `Package` of mark/gap timings becomes an envelope,
   and an envelope becomes IQ. Two nodes cover most of this list: OOK keying
   and two-level FSK. Both need edge shaping rather than hard switching, or
   the transmission splatters across the band: a raised-cosine ramp of a few
   microseconds is the difference between a legal signal and interference.

4. **Scheduling and limits.** Transmission is time critical in a way reception
   is not, so the graph needs to produce samples ahead of a deadline rather
   than in response to input. ISM bands also carry duty cycle limits (1% in
   parts of 868 MHz), and the transmitter should enforce them rather than
   leave it to the operator to remember.

A sensible first target is Morse keying on an amateur band into a dummy load:
it exercises the device, the modulator and the scheduler with no framing at
all, and it is unambiguously lawful with a licence.

## Suggested order

Cheapest first, by value per unit of work:

1. **PT2262/EV1527 fixed-code remotes.** One timing table each, the most
   common thing on 433.92 MHz, and the best test of the unknown-burst
   analyser, which should already be reading them as PWM.
2. **The rtl_433 weather station families.** Same shape as the Fine Offset
   decoder that exists: a parser and a CRC each.
3. **TPMS.** Short frames, plenty of them near any road, and the OOK and FSK
   variants exercise both banks.
4. **AIS.** The first protocol needing real framing (NRZI, bit stuffing,
   CRC16) but no new demodulator, and the results are immediately legible.
5. **POCSAG.** Adds BCH error correction, reusable afterwards.
6. **Wireless M-Bus T and C.** Common on 868 in Europe, and the sync word plus
   block CRC work carries over to Z-Wave and Homematic.
7. **The transmit path, ending in Morse.** Device, encoder, modulator,
   scheduler, proven end to end on the simplest possible protocol.
8. **LoRa.** The first new demodulator, self-contained, and it brings
   Meshtastic and LoRaWAN metadata with it.
9. **ADS-B.** Needs its own wideband chain rather than a bank channel, so it
   is a structural change: a scanner tier at 2 MS/s.

Everything below that (OFDM broadcast, trunked voice, cellular) is a project
each rather than a decoder each, and should be judged on its own.

# audio-timeline

Aligns two (or more) independently-clocked real-time audio streams onto a shared
timeline, absorbing small clock drift and hiding packet loss / discontinuities,
without losing sync over long recordings.

This is the common problem behind things like multi-track meeting recorders: you
capture "self" audio (e.g. a microphone) and "remote" audio (e.g. system/loopback
output) from two independent hardware clocks that drift relative to each other by a
few hundred parts-per-million. Naively concatenating each source's packets as they
arrive causes the two tracks to slowly drift out of sync over a long session, and any
dropped or corrupted packet needs to be accounted for so the tracks don't simply get
shorter.

`audio-timeline` factors this out as a small, dependency-light policy layer:

- Each packet's arrival time is defined against a monotonic host clock, so the number
  of samples that interval *should* contain can always be computed directly from host
  time, independent of the audio device's own (drifting) clock.
- If the actual sample count for a packet is close to that expectation (within
  [`MAX_SMOOTH_RATIO_DEVIATION`], 5% by default), the deviation is treated as ordinary
  clock drift and smoothed away with linear interpolation.
- If the deviation is large, or the packet is flagged discontinuous, it's treated as a
  real gap or dropped audio rather than drift, and corrected with a hard jump (silence
  padding or truncation) instead of a smooth resample — smoothing a genuine
  discontinuity would just spread the glitch out rather than fix it.
- A source that stops sending entirely (packet loss, a restart) is detected as a gap on
  the host clock and filled with silence, so downstream consumers never have to reason
  about missing time.

Two independently-drifting sources fed through their own [`TimelineAligner`] end up on
the same timeline without the two aligners ever needing to talk to each other — each
just needs an accurate host-clock timestamp per packet.

## Usage

```rust
use audio_timeline::{AudioPacket, TimelineAligner};

let mut aligner = TimelineAligner::new(48_000); // nominal sample rate

aligner.ingest(&AudioPacket {
    host_time_ns: 0,
    nominal_duration_ns: 20_000_000, // 20ms
    samples: vec![0.0; 960],
    discontinuity: false,
});

// ... feed subsequent packets as they arrive ...

// At the end of a session, pad up to a known end time so two independently-aligned
// tracks come out the same length even if one lost its final packets.
aligner.finalize_up_to(2 * 60 * 1_000_000_000);

let track: Vec<f32> = aligner.into_output();
```

See [`xcorr`] for a way to independently measure how well two aligned tracks stayed in
sync (e.g. by injecting a known marker into both sources and measuring the lag between
them after alignment) — useful in tests and diagnostics.

## What this crate does *not* do

The resampling step itself is a simple linear interpolation, not a sinc/FFT-based
resampler — real-world clock drift is small enough (parts-per-million) that this is
sufficient in practice, and it keeps the crate free of heavier dependencies. If you
need higher-fidelity resampling, consider resampling the final output with something
like [`rubato`](https://docs.rs/rubato); note that rubato's API is built around
continuously streaming fixed-size chunks with a runtime-adjustable ratio, so pairing it
with this crate's per-packet model would need a small adapter rather than a drop-in
swap.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or
  <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.

# Architecture

The router has one job: take a single stereo source and deliver it, sample-rate converted and volume controlled, to several PipeWire outputs at once. This document describes how the running process is organized, where latency comes from, and which tradeoffs are deliberate.

## Process model

| Thread | Owns | Blocking work |
| --- | --- | --- |
| GPUI main | Window, view state, commands | None; rendering never spawns processes or touches the audio rings |
| Control worker | Device discovery, stream routing, OS volume, config saves, startup and shutdown | `pactl`, JSON serialization, file IO |
| Capture worker | Reading the hub stream, distributing frames to output rings | `parec` or `pw-record` stdout |
| Telemetry worker | Meters, spectrum, correlation-free stereo analysis | Bounded channel receive |
| Render worker per output | Reading one ring, resampling, writing playback stdin | `pacat` or `pw-play` stdin |

The control worker is the only thread that mutates routing state. It publishes immutable snapshots, and the UI reads the latest snapshot when it paints. Startup and shutdown run off the UI thread, so the window stays responsive while modules load or unload.

## Audio path

1. `pactl load-module module-null-sink` creates `pw_matrix_virtual_sink` at 48 kHz stereo float, unless that sink already exists.
2. Capture reads that sink's monitor in 256-frame chunks at 48 kHz.
3. Each enabled output has its own ring. Capture writes the same frames to every ring.
4. Each output worker reads its ring, resamples from 48 kHz to the device rate, applies gain and the limiter, and writes to its playback process.
5. A single telemetry copy is sent to the analysis worker. Analysis never runs on the capture thread.

Rings are strictly single producer, single consumer. The producer writes frames and advances the write cursor. Only the consumer advances the read cursor, so no cursor is shared for mutation. Samples are stored as atomic float bits, and each consumer keeps its own interpolation history window, which avoids shared mutable filter state.

On overload the producer keeps the frames already in the ring and discards the newest excess. That choice never corrupts unread audio, unlike overwriting the oldest frames.

## Resampling

Output rates differ from the 48 kHz hub, so every output converts independently.

- Filter: causal 128-tap Blackman-windowed sinc, evaluated from shared lookup tables for the sinc and window.
- Coefficients: 1024 interpolated phase steps per rate pair, normalized to unity DC gain, shared between outputs with the same rate pair and built before streaming starts.
- Cutoff: 95% of the lower Nyquist frequency, based on the conservative upper servo ratio. Transition width is fixed in source-frequency units, so extreme downsampling ratios keep less rejection near the destination Nyquist limit.
- Delay: 63 source frames, 1.31 ms at 48 kHz and 1.43 ms at 44.1 kHz. The delay is a deliberate quality cost.
- History: each consumer keeps 127 source frames so a block boundary does not reset the filter, and the window rotates as samples are consumed instead of reloading taps per sample.
- Drift: long-term ratio correction follows ring occupancy with bounded correction, so the resampler absorbs small clock differences without unbounded buffer growth.

The previous implementation used cubic Hermite interpolation, which let a 23 kHz tone fold into the output at only 3.1 dB of attenuation. The windowed-sinc path measures 96.6 dB attenuation on the same test.

## Limiting and gain

`soft_limit` passes samples up to 0.99, then follows a quadratic knee with matching slope and reaches full scale at 1.01 with zero slope. The function is continuous, monotone, odd, and bounded. Non-finite samples are silenced.

Volume smoothing uses a time constant expressed in milliseconds and converted per sample rate, so the same control move sounds the same at 44.1 kHz and 96 kHz.

## Latency accounting

Displayed and stored latency values are estimates of the software path:

- Requested floor: user control, minimum 6 ms in the interface.
- Effective ring floor: 8 ms minimum, because the control worker clamps lower requests.
- Playback buffer: 12 ms requested from `pacat` or `pw-play`, used as a uniform proxy for every discovered output.
- Filter delay: added to absolute estimates. It cancels out in relative comparisons between outputs, because every output converts from the same 48 kHz source.

The app labels these values as estimates. It does not query live DAC latency, and it does not claim that two outputs are phase aligned.

## Lifecycle safety

- The router refuses to start when `pw_matrix_virtual_sink` already exists, and changes nothing in that case.
- Module IDs created by the router are recorded. Unload removes only those IDs.
- The previous default sink is captured before a takeover and restored only if the router still owns the default at shutdown.
- Configuration writes go to a temporary file, then flush, sync, and rename, so a crash does not leave a truncated config.
- Shutdown runs after the GPUI event loop returns, because GPUI waits only 200 ms for quit observers.

## Telemetry

Three owned stereo blocks circulate between capture and the analysis worker. If analysis falls behind, capture drops the telemetry copy for that chunk and keeps delivering audio. Snapshots are published under a mutex that the UI reads only when painting.

The spectrum window is a cached 256-sample frame with 16 bands. Peak holds decay on the analysis path. Stereo correlation is computed for the model but not displayed; the interface shows balance and the L/R level bars.

## References

- Julius O. Smith, [Digital Audio Resampling](https://ccrma.stanford.edu/~jos/resample/Implementation.html)
- Putnam and Smith, [Design of Fractional Delay Filters Using Convex Optimization](https://ccrma.stanford.edu/~jos/pdf/optfir.pdf)
- Giannoulis, Massberg, and Reiss, [Digital Dynamic Range Compressor Design: A Tutorial and Analysis](https://joshreiss.github.io/documents/2012/GiannoulisMassbergReiss-dynamicrangecompression-JAES2012.pdf)
- PortAudio, [Writing a callback](https://www.portaudio.com/docs/v19-doxydocs/writing_a_callback.html)
- PipeWire, [pipewire.conf(5)](https://docs.pipewire.org/page_man_pipewire_conf_5.html)
- Cargo, [Profiles](https://doc.rust-lang.org/cargo/reference/profiles.html)
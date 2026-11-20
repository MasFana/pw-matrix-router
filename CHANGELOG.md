# Changelog

All notable changes to this project are recorded here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-11-20

First public version of the Rust and GPUI application for Linux.

### Added

- Native GPUI window with an output table, per-output routing, volume, mute, extra delay, and auto-sync.
- Virtual sink creation and capture from a 48 kHz stereo hub, with one single-producer, single-consumer ring per output.
- Adaptive windowed-sinc resampling to each output's own rate, with shared phase tables and per-output history.
- Stereo meters with peak hold, a 16-band spectrum, and a balance bar.
- Config persistence in `~/.config/pw-matrix-router/config.json`, including import of the earlier Go config layout when present.
- Default sink restore and owned-module cleanup on shutdown, plus signal handling for SIGINT, SIGTERM, and SIGHUP.
- Keyboard control of selection, routing, mute, volume, buffer floor, delay, auto-sync, refresh, and quit.

### Changed

- Replaced cubic interpolation with a 128-tap windowed-sinc filter. A 23 kHz tone that folded into the output at -3.1 dB is now attenuated by -96.6 dB at 48 kHz to 44.1 kHz.
- Replaced the discontinuous soft limiter with a continuous, monotone knee.
- Volume smoothing now uses a millisecond time constant instead of a fixed per-sample coefficient.
- Telemetry analysis moved to its own worker with recycled buffers, so overload skips telemetry instead of delaying audio.
- Release profile uses thin LTO and one codegen unit.

### Fixed

- Rejected stream moves no longer abort startup or block later migrations; each failure is reported and the run continues.
- Removing the correlation readout from the interface also removed its numeric formatting path.

### Removed

- The cubic Hermite interpolation module and its tests.
- Correlation visualization. Balance remains.

[Unreleased]: https://github.com/MasFana/pw-matrix-router/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/MasFana/pw-matrix-router/releases/tag/v0.1.0
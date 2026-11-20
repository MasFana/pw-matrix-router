# Developing

## Toolchain

The project pins `nightly-2026-11-18` in `rust-toolchain.toml`. Stable Rust currently fails inside `gpui-pre 0.3.12` on `std::hint::cold_path`, so keep the pin and install the components rustfmt and clippy need:

```sh
rustup toolchain install nightly-2026-11-18 --component rustfmt,clippy,rust-analyzer
```

## Checks

```sh
cargo test --lib --locked
cargo fmt --check
cargo clippy --all-targets --locked
cargo build --release --locked --bin pw-matrix-router
```

Tests cover the DSP kernels, config migration and atomic saves, routing migration behavior, latency accounting, and GPUI component tests for the output table and controls. The GPUI tests need an X or Wayland session; on a headless machine run them under `xvfb-run -a`.

## Manual smoke test

Do not run this against a machine where something else already owns `pw_matrix_virtual_sink`. The router refuses to start in that case, which is the intended behavior. The procedure below creates only test-owned sinks and uses a throwaway config directory.

Expect the app to become the default sink and to move existing playback streams onto the hub. It restores the previous default on a clean exit, so quit with Ctrl+Q and check afterwards. A hard kill leaves the takeover behind.

For a run that must not touch the default sink or existing streams, drive the library directly with a small harness:

```rust
let mut options = EngineOptions::default();
options.virtual_sink_name = format!("pw_matrix_test_{}", std::process::id());
options.manage_default = false;
options.move_existing_streams = false;
options.config_path = Some(std::env::temp_dir().join("pw-matrix-test.json"));
let engine = AudioEngine::with_options(options);
```

1. Create two test outputs at different rates:

   ```sh
   pactl load-module module-null-sink sink_name=pw_matrix_test_48 rate=48000 channels=2 format=float32le
   pactl load-module module-null-sink sink_name=pw_matrix_test_44 rate=44100 channels=2 format=float32le
   ```

2. Start the router with an isolated config so your real settings stay untouched:

   ```sh
   XDG_CONFIG_HOME=$(mktemp -d) cargo run --release --locked
   ```

3. Route both test outputs and play a stereo tone into the hub:

   ```sh
   pacat --playback --raw --device=pw_matrix_virtual_sink --format=float32le --rate=48000 --channels=2 tone.f32
   ```

4. Record each test monitor and check levels:

   ```sh
   parec --raw --device=pw_matrix_test_44.monitor --format=float32le --rate=44100 --channels=2 --latency-msec=8 > out44.f32
   ```

5. Quit with Ctrl+Q, then confirm the previous default sink is unchanged and the router's own hub is gone:

   ```sh
   pactl get-default-sink
   pactl list short sinks
   pactl unload-module <id printed in step 1>
   ```

## Measuring the DSP path

Benchmarks belong in throwaway harnesses, not in the crate. Compile a small binary against `src/dsp` with the same pinned toolchain, warm up, and use `std::hint::black_box` on inputs and outputs. Report:

- passband error near 20 kHz at the destination rate
- alias rejection for a tone above destination Nyquist
- group delay in source frames
- nanoseconds per output frame for the full ring write and read path

Single-run numbers are noise-prone. Repeat the run and report the spread.

## Profiling

- `perf record -g` on the capture, render, and telemetry threads shows whether resampling or process IO dominates.
- `pw-top` shows PipeWire graph quantum, per-stream xruns, and the driver latency actually in use.
- The app's own footer and **Audio details** dialog show ring occupancy, underruns, and the latency estimates it used.

## Gotchas

- The capture process prefers `parec` and falls back to `pw-record`. Both are started with `--latency=256`, a sample count at the fixed 48 kHz rate.
- Playback requests `--latency-msec=12` for `pacat` and `--latency=12ms` for `pw-play`.
- Worker priority is best effort. Child processes are spawned after the worker raises its own priority, so the children do not inherit a realtime class.
- Rendering never starts subprocesses. If you add a control path, put it on the worker and publish a snapshot.
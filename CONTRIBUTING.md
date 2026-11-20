# Contributing

## Before you start

- Open an issue for behavior changes, new controls, or anything that touches device ownership. The router manipulates the user's default sink, so those paths get extra review.
- Small fixes (typos, docs, a broken test) can go straight to a pull request.

## Setup

```sh
rustup toolchain install nightly-2026-11-18 --component rustfmt,clippy
cargo test --lib --locked
```

## Rules for audio changes

- No allocation, file IO, or subprocess work on the capture or render path. Steady-state audio code should not allocate.
- Keep the rings single producer, single consumer. A consumer alone advances its read cursor.
- Never remove or reconfigure sinks, modules, or streams the router did not create.
- Preserve the previous default sink around a takeover, and restore it only when the router still owns the default.
- Latency values are estimates. Do not label them as measured DAC latency or phase alignment.
- Do not lower the effective buffer floor or the requested playback buffer without measurements showing underrun behavior on real hardware.

## Code and tests

- Match the surrounding style. The crate is Rust 2024.
- Add a regression test when you fix a defect that a test can catch. Avoid tests that assert wiring, default constants, or rendered source text.
- Run before pushing:

  ```sh
  cargo fmt
  cargo clippy --all-targets --locked
  cargo test --lib --locked
  ```

- Keep documentation in `docs/` and update it when behavior changes.

## Pull requests

Describe what changed, why, and how you verified it. For audio work, include the output device rates you tested and whether either endpoint reported underruns. Screenshots help for interface changes.

## Commit messages

Use a short imperative subject, then a body that explains the reason. One logical change per commit.

## License

By contributing, you agree that your work is licensed under the MIT license in [LICENSE](LICENSE).
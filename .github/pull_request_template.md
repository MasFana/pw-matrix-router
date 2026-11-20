## What changed

<!-- One or two sentences. Name the behavior, not the file. -->

## Why

<!-- The user-visible problem or the reason for the change. -->

## Verification

<!-- Commands you ran, and for audio changes: output devices, rates, and underruns. -->

- [ ] `cargo fmt --check`
- [ ] `cargo clippy --all-targets --locked`
- [ ] `cargo test --lib --locked`
- [ ] Manually checked the changed interface or audio path

## Checklist

- [ ] No allocation, file IO, or subprocess work added to the capture or render path
- [ ] Rings remain single producer, single consumer
- [ ] No sink, module, or stream is touched unless the router created it
- [ ] Documentation updated when behavior changed
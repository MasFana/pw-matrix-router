# Releasing

## Repository settings

Set these once, in the GitHub repository settings.

Description:

> PipeWire audio router for Linux. One stereo source to many outputs at their own sample rates, with per-output volume, delay, and a native GPUI window.

Topics:

```
pipewire audio-router audio-splitter multi-output-audio linux rust gpui dsp
sample-rate-conversion resampler virtual-sink low-latency desktop-app pipewire-pulse
```

Website: leave empty until a documentation page exists.

Social preview: upload `docs/screenshots/router-window-dark.png` in **Settings → General → Social preview**.

## Version bump

1. Update `version` in `Cargo.toml`. Run `cargo build --locked` so `Cargo.lock` matches.
2. Move the `Unreleased` entries in [CHANGELOG.md](../CHANGELOG.md) under the new version with today's date.
3. Run the checks from [DEVELOPING.md](DEVELOPING.md).
4. Commit as `release: vX.Y.Z`.
5. Tag and push:

   ```sh
   git tag -a vX.Y.Z -m "vX.Y.Z"
   git push origin master --follow-tags
   ```

`release.yml` builds the binary on the tag, writes a `.tar.gz` with the binary, README, and LICENSE, generates `sha256sums.txt`, and attaches both to the GitHub release. Review the draft notes, then publish.

## Before the first public push

- Confirm `git remote -v` points at the real repository, and that `Cargo.toml` `repository`, `homepage`, and the README clone URL match it.
- Check that `.gitignore` excludes `target/`, `.codegraph/`, `.agents/`, and `skills-lock.json`.
- Decide whether the repository should carry the MIT license as written. Changing it later is harder once contributors appear.
- Publish the first release as `v0.1.0` and mark it as a pre-release, since the audio path has only been verified on one machine.

## Release checklist

- [ ] `cargo test --lib --locked` passes
- [ ] `cargo build --release --locked` passes
- [ ] The smoke test in DEVELOPING.md passes on a machine with two outputs at different rates
- [ ] CHANGELOG.md describes the user-visible changes
- [ ] The screenshot in `docs/screenshots/` matches the current interface
- [ ] No local config, logs, or audio captures are committed
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

`release.yml` builds the binary on the tag and runs `packaging/build.sh`, which produces:

- `pw-matrix-router_<version>_amd64.deb` for Debian-based distributions
- `pw-matrix-router-<version>-1.x86_64.rpm` for Fedora-based distributions
- `pw-matrix-router-<version>-x86_64.AppImage` for any distribution
- `pw-matrix-router-<version>-x86_64-linux.tar.gz` with the binary, docs, and desktop entry
- `sha256sums.txt` covering every file

The workflow opens a draft release with all assets. Review the notes, then publish.

## Packaging locally

```sh
cargo build --release --locked
bash packaging/build.sh --out dist
```

Each format needs its tool: `dpkg-deb`, `rpmbuild`, and `appimagetool`. Pass `--formats tarball,deb` to build a subset. `APPIMAGETOOL=/path/to/appimagetool` overrides the tool lookup.

Desktop entry, AppStream metadata (`*.appdata.xml`, the name AppImage tooling looks for), and the icon source live in `packaging/`. Regenerate the PNG icons after editing the SVG:

```sh
for s in 64 128 256 512; do
  rsvg-convert -w $s -h $s packaging/pw-matrix-router.svg -o packaging/icons/pw-matrix-router-$s.png
done
```

## Before the first public push

- Confirm `git remote -v` points at the real repository, and that `Cargo.toml` `repository` and the README clone URL match it.
- Check that `.gitignore` excludes `target/`, `.codegraph/`, `.agents/`, and `skills-lock.json`.
- Decide whether the repository should carry the MIT license as written. Changing it later is harder once contributors appear.
- Publish releases as normal releases, so `releases/latest` and stable download links work. Mark a build as a pre-release only when it ships without the smoke test in DEVELOPING.md or when notes say a path is unverified.

## Release checklist

- [ ] `cargo test --lib --locked` passes
- [ ] `cargo build --release --locked` passes
- [ ] The smoke test in DEVELOPING.md passes on a machine with two outputs at different rates
- [ ] CHANGELOG.md describes the user-visible changes
- [ ] Every package from `packaging/build.sh` launches and restores the default sink on exit
- [ ] The screenshot in `docs/screenshots/` matches the current interface
- [ ] No local config, logs, or audio captures are committed
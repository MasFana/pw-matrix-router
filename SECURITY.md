# Security policy

## Reporting

Report vulnerabilities privately through GitHub's **Report a vulnerability** button on the Security tab. Do not open a public issue for a security problem.

Include the version, your distribution, your PipeWire version, and the steps that reproduce the problem. Expect an initial reply within a week.

## Scope

In scope:

- Commands the router runs with user privileges, and the arguments it builds from device or stream names.
- Handling of configuration files, including imports from the earlier Go layout.
- Module ownership: loading, tracking, and unloading what the router created.
- Panics that could leave a module loaded, a default sink changed, or a capture process orphaned.

Out of scope:

- Audio glitches or dropouts without a memory-safety or privilege cause.
- Problems in PipeWire, PulseAudio compatibility tools, or the desktop session.
- Anything that requires the attacker to already control the user's session.

## What the router does to your system

For transparency, these are the state changes it makes while running:

- Loads a null sink module for the hub and unloads only modules it loaded.
- Makes the hub the default sink and moves existing playback streams to it on start, then restores the earlier default on a clean exit.
- Writes `~/.config/pw-matrix/config.json` through a temporary file and an atomic rename.

It does not run as root, does not install services, and does not download code at runtime.
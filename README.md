> Last tested working 9/27/2026

# LAN Audio Share

Streams audio between two computers across a LAN.
Runs on MacOS and Windows.

## Building From Source

Needs Rust toolchain [rustup.rs](https://rustup.rs)

**Windows**
`cargo build --release`
Appears at `.\target\release\lan-audio-share.exe`

**macOS**
`bash scripts/make-macos-app.sh`
Appears at `./target/release/LAN Audio Share.app` — double-click it or drag it to /Applications.
Add `--universal` to also run on Intel Macs.

Allow the Local Network and Microphone prompts on first launch. Without them the app can't find peers or capture audio.

Use along with [Loopback](https://rogueamoeba.com/loopback/) or [Virtual Audio Cable](https://vac.muzychenko.net/en/download.htm) to share desktop audio.

Inspired by [SonoBus](https://github.com/sonosaurus/sonobus), but doesn't need a central server.

## License
This repository and all contained code is licensed under GNU GPLv3. See .\LICENSE.txt for more info.

## Disclosure

> **Note**: This entire project was written by AI (Claude Code: Opus 5). All code, architecture decisions, and implementation details were generated through AI assistance.
>
> I am a strong advocate for never mixing generated code into real repos.
> Projects like these should clearly disclose as such.
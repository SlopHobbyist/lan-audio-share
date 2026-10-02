> Last tested working 9/27/2026

# LAN Audio Share

![alt text](https://raw.githubusercontent.com/SlopHobbyist/thumbnails/main/lan-audio-share.png "Example Screenshot | UI sending audio on Windows 10")

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
Builds universal (Apple Silicon + Intel) by default. Add `--native` for a quicker build that only runs on your own Mac.

Allow the Local Network and Microphone prompts on first launch. Without them the app can't find peers or capture audio.

Use along with [Loopback](https://rogueamoeba.com/loopback/) or [Virtual Audio Cable](https://vac.muzychenko.net/en/download.htm) to share desktop audio.

## Media Keys

Optional, off by default. Tick **Forward media keys** under *Advanced* on both machines and your play/pause, next, previous and stop keys control the computer the audio is coming from instead of the one you're sitting at.

Volume keys are left alone — those stay local. On macOS, capturing the keys needs Accessibility permission (System Settings > Privacy & Security > Accessibility).

Inspired by [SonoBus](https://github.com/sonosaurus/sonobus), but doesn't need a central server.

I made the icons myself in Adobe Illustrator.

## License
This repository and all contained code is licensed under GNU GPLv3. See .\LICENSE.txt for more info.

## Disclosure

> **Note**: This entire project was written by AI (Claude Code: Opus 5). All code, architecture decisions, and implementation details were generated through AI assistance.
>
> I am a strong advocate for never mixing generated code into real repos.
> Projects like these should clearly disclose as such.
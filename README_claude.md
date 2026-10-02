# LAN Audio Share

Streams audio between two computers on a local network. One window, two
choices: send or receive, and which device. It saves what you picked and resumes
it the moment you launch it again — there is no server, no session to join, and
nothing to unmute.

Built for sending desktop audio from an ARM MacBook to a Windows PC, but nothing
in it is specific to that direction.

## Building

Needs a Rust toolchain ([rustup.rs](https://rustup.rs)). The same command works
on macOS and Windows; build it on each machine, since the binary is native.

```sh
cargo build --release
```

The result is `target/release/lan-audio-share` (`.exe` on Windows). Run it
directly — there is nothing to install.

Use the release build for actual listening. Debug builds are set to `opt-level =
1` so they can at least keep up with a real-time audio callback, but the release
build is what you want.

## Using it

Launch it on both machines and pick:

- **On the machine with the audio** — `SEND`, then choose an input device. On
  macOS pick `Loopback Audio` (or whichever virtual device carries your desktop
  sound); that is what turns system audio into something selectable here. On
  Windows, WASAPI loopback devices show up in the same list.
- **On the machine you want to hear it** — `RECEIVE`, then choose an output
  device.

That is the whole setup. The receiver announces itself on the network, the
sender picks it up within about half a second and starts streaming. Either end
can be restarted, in any order, and the link comes back on its own. Settings are
written to disk as you change them, so next launch skips all of the above.

The status area tells you whether it is actually working — who it is streaming
to or playing from, a level meter, and the buffer and drift figures.

## Media keys

Off by default. Turn **Forward media keys** on under *Advanced* — on **both**
machines — and the play/pause, next, previous and stop keys on the machine you
are listening at press themselves on the machine the audio is coming from. That
is usually what you meant: the computer with the music on it is the one across
the room.

While it is on, the listening machine stops seeing those keys itself. One press
should not pause two computers, and the press was meant for the other one.

Volume keys are deliberately left alone. Your own volume keys belong to your own
speakers, and the sender's system volume sits upstream of the capture device, so
forwarding them would change the level of the stream rather than how loud it is
in the room. Use the **Volume** slider for that.

It needs no new firewall rule: the key goes to the same discovery port the two
machines already use to find each other, from the same socket the audio arrives
on. A sender only accepts keys from a listener it is actually streaming to, drops
anything from its own addresses (two instances on one machine would otherwise
bounce a single press between them forever), and refuses more than one press per
50 ms, since each one synthesises a real keypress.

Per-platform notes:

- **Windows** claims the keys with `RegisterHotKey`, which hands them over
  exclusively. If another program got there first, the status area says which key
  it could not get, and the rest still work.
- **macOS** needs Accessibility to *capture* — System Settings → Privacy &
  Security → Accessibility — because the event tap also swallows the press.
  Pressing needs no permission. There is no system-defined stop key on macOS, so
  stop is the one key that cannot be forwarded to a Mac; next and previous arrive
  as the fast-forward and rewind keys, which is what the keys on an Apple
  keyboard send.

## Latency

Defaults are 48 kHz with a 2048-frame buffer, which works out to roughly:

| | |
|---|---|
| capture buffer (2048 frames) | ~43 ms |
| network | <1 ms on a wired LAN |
| receive jitter buffer | 60 ms |
| playback buffer (2048 frames) | ~43 ms |
| **total** | **~145 ms** |

If you want it tighter, lower **Buffer size** on *both* machines and drop
**Receive buffer** to match. 256 frames with a 25 ms receive buffer lands around
35 ms end to end, at the cost of being more sensitive to a busy CPU or a noisy
Wi-Fi link.

One rule if you change these: the receive buffer should be at least as long as
the sender's capture buffer. Audio does not exist until the sender's callback
fires, so it necessarily arrives in bursts that size, and the receive buffer is
what absorbs them. It will raise its own target automatically if it underruns,
but starting too low means hearing a few glitches first.

## How it works

Audio goes over UDP as raw PCM — no codec. On a LAN the bandwidth is free
(48 kHz stereo float is about 3 Mbit/s), and skipping compression removes the
encoder's lookahead delay entirely, so it is both simpler and lower-latency than
a compressed transport. Packets are 160 frames each, sized to fit in one
datagram so nothing gets IP-fragmented.

Discovery is peer-to-peer. The receiver repeats a small beacon over both
multicast and subnet broadcast, because networks that quietly drop one often
pass the other; the sender collects those and unicasts audio to each. Unicast
audio survives Wi-Fi far better than multicast audio would.

The part that makes a stream hold up over hours is the pair of mechanisms on the
receiving end:

- A **jitter buffer** absorbs network jitter and the sender's bursty callback
  timing. It fills to its target before playback starts, conceals lost packets
  with exactly the right number of frames so everything after stays in sync, and
  grows its own target if it ever runs dry.
- **Clock drift correction** deals with the fact that two machines both running
  at "48 kHz" are not running at the *same* 48 kHz. Their crystals differ by
  tens of parts per million, so a fixed buffer either drains or backs up over
  minutes. Playback rate is continuously nudged — by well under half a percent,
  smoothed heavily enough to be inaudible — to hold the buffer at its target.
  The same resampler handles genuinely different sample rates on the two ends for
  free.

Both are ideas worth borrowing from SonoBus; without the second one, a stream
glitches every few minutes no matter how big the buffer is.

## If it does not connect

The two machines need to reach each other on UDP ports 47771 (discovery) and
47772 (audio).

**Windows** will usually prompt on first launch. If it did not, or you dismissed
it, allow it explicitly from an admin terminal:

```powershell
netsh advfirewall firewall add rule name="LAN Audio Share" dir=in action=allow protocol=UDP localport=47771,47772
```

**macOS** asks for Local Network access on first launch — that prompt has to be
accepted, or the beacons go nowhere. It is under System Settings → Privacy &
Security → Local Network if you need to re-check it.

If discovery still does not work — some networks block multicast *and*
broadcast, and it cannot work across subnets — put the receiver's IP address in
**Send directly to** under Advanced on the sending machine. That bypasses
discovery entirely.

### Reading the numbers

- **buffer** should sit near your configured receive buffer and stay there.
- **clock drift** is the correction being applied. Anything up to a few hundred
  ppm is normal and is the mechanism working. Pegged at ±4000 ppm means it has
  hit its limit and something other than clock drift is wrong.
- **glitches** counts underruns, concealed frames and late packets. All three
  should stay at zero on a wired LAN. Late packets moving means the network is
  reordering; lost frames means it is dropping.

Settings live at `%APPDATA%\LanAudioShare\config\config.json` on Windows and
`~/Library/Application Support/LanAudioShare/config.json` on macOS. Deleting
that file resets everything.

## Tests

```sh
cargo test
```

The suite covers the wire format, the resampler and drift controller, the jitter
buffer's behaviour around startup, underruns and resyncs, packetization, and who
is allowed to press a media key here. Three of the tests use real sockets and the
machine's real audio hardware: one streams audio through the full receive path
into the default output device, one checks that a sender discovers a receiver with
no configuration, and one confirms a dropped packet is concealed at exactly the
right length. A fourth uses the real OS input APIs — it claims this machine's
media keys, presses one, and checks the claim caught it. They skip themselves with
a message when the device, or the key, is not available.

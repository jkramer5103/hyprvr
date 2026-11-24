# hyprvr

**A multi-screen VR desktop for Hyprland on Meta Quest 3.** Put on the headset and your Linux
laptop gets extra virtual monitors that float around you in sharp, low-latency panels. Use
your normal keyboard and touchpad, or point and pinch with controllers or your hands.

> [!NOTE]
> **🤖 Built and maintained by AI.** Every line of code, the docs and this README were
> written by AI coding agents (Claude). A human gives the direction, tests it on real
> hardware and approves the changes.
>
> **✅ It works.** It's used on real hardware (ThinkPad X280 + Quest 3 over Wi‑Fi): multiple
> screens, video playback, audio, hand tracking and passthrough. Expect some rough edges,
> and feel free to open issues.

## What you get

- **Real Hyprland monitors**: each panel is a headless output, so windows, workspaces and
  `movefocus` work as usual.
- **Sharp text**: still content is sent as lossless tiles, and only moving areas (video,
  scrolling) are hardware‑encoded with VAAPI H.264.
- **Arrange panels in 3D**: grab, move, resize and curve them. Hyprland's monitor layout
  follows what you see.
- **Input**: laptop keyboard and touchpad, controller laser, or hand pinch.
- **Audio** can play on the headset or the laptop (toggle with `vrd audio`).
- **Passthrough** with dimming, or 360° image environments.
- **Secure**: QUIC + TLS 1.3, and you pair the headset with a one‑time PIN.

## Requirements

| | |
|---|---|
| Laptop | Linux running **Hyprland**, Intel/AMD GPU with VAAPI. The installer is built for Arch / Omarchy. |
| Headset | **Meta Quest 3** with [developer mode](https://developers.meta.com/horizon/documentation/native/android/mobile-device-setup/) turned on |
| Network | Both on the same Wi‑Fi/LAN (5 GHz or Wi‑Fi 6 recommended), or Tailscale |
| To build the Quest app | Android SDK + NDK (`ANDROID_HOME`, default `/opt/android/sdk`), a JDK. `setup.sh` installs `cargo-ndk` and the Rust target for you. |

## Setup

```bash
git clone https://github.com/jkramer5103/hyprvr.git
cd hyprvr
./setup.sh
```

That's it. `setup.sh`:

1. installs system packages, Rust and the `vrd` daemon, opens the firewall for your LAN
   (UDP 47800–47801) and starts `vrd` as a systemd user service;
2. if the Quest is plugged in over USB (adb), it builds the headset app, installs it and
   gives it your laptop's IP.

Is the headset not connected yet? Plug it in, accept the USB debugging prompt in the
headset and run `./setup.sh quest`.

Then put on the headset, open **hyprvr** from *Library → Unknown Sources* and type the PIN
shown by:

```bash
vrd pin      # also shown as a desktop notification
```

You only need to pair once.

<details>
<summary>Setup options</summary>

```bash
./setup.sh laptop          # only the daemon
./setup.sh quest           # only the headset app
./setup.sh --tailscale     # also allow connections over Tailscale
./setup.sh --no-firewall   # don't touch ufw
./setup.sh --no-deps       # skip pacman
```

Not on Arch? Install ffmpeg (with headers), libva, clang, pkg-config, libpulse and libnotify
yourself, then run `./setup.sh --no-deps`.
</details>

## Everyday use

```bash
vrd status                 # connection, screens, bitrate
vrd screen add 2560x1440   # add a virtual screen
vrd screen remove 2
vrd mirror eDP-1           # show a real monitor in VR
vrd audio toggle           # headset ↔ laptop audio
vrd layout reset           # panels back to the default arc
vrd unpair all
```

**Bar widget**: `vrd waybar` prints Waybar JSON. `./scripts/install-laptop.sh` prints
ready‑to‑paste Waybar and Quickshell snippets at the end.

**Config**: `~/.config/hyprvr/config.toml` (codec, bitrate, default screens, …).
**Environments**: put equirectangular `.jpg`/`.png` files in `~/.config/hyprvr/environments/`
(`vrd env dir` prints the folder). Or get 10 free ones: press **Download** in the headset's
environment picker (shown while the folder is empty), or run `vrd env fetch`. They are not
bundled. hyprvr downloads the originals from Poly Haven (CC0), Wikimedia Commons and ESO
(CC BY / CC BY-SA, ~250 MB), converts them locally (~65 MB), and writes `CREDITS.md` next to
them. `vrd env credits` lists the photographers and licenses. The headset shows the credit of
the current one in the picker.

## Troubleshooting

| Problem | Fix |
|---|---|
| Headset doesn't find the laptop | Same network? Check `systemctl --user status vrd` and that UDP 47800/47801 are open. Run `./setup.sh quest` again to set the IP. |
| Blocky or laggy video | Use 5 GHz Wi‑Fi, or lower `video_kbps_1080p` in the config. |
| Daemon logs | `journalctl --user -u vrd -f` |
| Headset logs | `quest/scripts/install-quest.sh` (installs, launches and follows logcat) |

## How it works

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) and [docs/PROTOCOL.md](docs/PROTOCOL.md).
In short: `vrd` (Rust) creates headless Hyprland outputs, captures them with
wlr‑screencopy, sends lossless tiles or H.264 over QUIC, and injects input through a
virtual pointer. The Quest app (Rust, OpenXR, GLES) shows each screen as a composition
layer.

```
crates/vrd      laptop daemon + CLI
crates/proto    shared wire protocol, tile codec, PIN pairing
crates/probe    test client
quest/          headset app (Rust → APK)
```

## License

[MIT with the Commons Clause](LICENSE). Use it anywhere, including at work, and change
and share it freely. The one thing you can't do without permission is **sell it**, that is
charge money for hyprvr itself or for a product or service that's mainly hyprvr. Open an
issue if you want to ask.

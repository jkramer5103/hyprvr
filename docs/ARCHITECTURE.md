# hyprvr architecture

Multi-screen VR desktop for Hyprland on Meta Quest 3. Primary target: ThinkPad X280
(i5-8350U, UHD 620 / Gen9.5: H.264 + HEVC 8-bit 4:2:0 VAAPI encode only), laptop on Wi-Fi.

```
 Hyprland ──headless outputs──► vrd (Rust daemon, systemd user service)
   ▲  ▲                          ├─ capture: wlr-screencopy (SHM, with damage), cursor hidden
   │  │                          ├─ hybrid encoder per screen: lossless tiles | VAAPI H.264 region
   │  └─ wlr-virtual-pointer ◄───├─ input from headset (laser / pinch)
   │                             ├─ Hyprland IPC: outputs, monitor positions, cursorpos
   └── IPC (hyprctl socket) ◄────├─ audio: null sink monitor -> PCM datagrams (switchable)
                                 └─ QUIC (quinn, TLS 1.3, PIN-pairing w/ SPAKE2)
                                         │ Wi-Fi
                                         ▼
                         Quest app (Rust, OpenXR, GLES)
                          ├─ per screen: RGBA canvas texture  ◄─ tiles (zstd) + MediaCodec video
                          ├─ quad / cylinder composition layers (sharp text, 90 Hz head motion)
                          ├─ passthrough + dim, environments, keyboard passthrough cutout
                          └─ controllers / hand pinch laser, grab bars, toolbar
```

## Hybrid encoding (per screen)
* Damage from screencopy -> `TileDiffer` finds tiles (64x64) that really changed.
* Small/infrequent changes -> lossless tiles (`proto::tiles`, delta filter + zstd).
* Sustained motion (many tiles/frame over several frames, e.g. video, scrolling, games)
  -> a **video region**: the 16-aligned bounding box of the moving tiles is encoded with
  VAAPI H.264 (low-latency, no B-frames, infinite GOP, IDR on demand). Region change with
  hysteresis -> new `VideoStart` + IDR.
* Motion stops for ~150 ms -> `VideoStop`, then lossless tiles over the region (sharpen).
* Idle screens send nothing.

## Layout sync
Panels are placed in 3D by the headset. The daemon converts the 3D arrangement to
Hyprland monitor positions (`keyword monitor NAME,WxH@60,X x Y,1`): panels sorted by azimuth
form a row; a panel clearly above/below a neighbour stacks vertically. The physical eDP stays
a normal monitor, placed centered under the row (where the laptop physically is), so mouse
edges and `movefocus` match what you see.

## Input
* Laptop keyboard / touchpad drive Hyprland natively (zero added latency).
* Laser (controller aim pose) or hand pinch (XR_EXT_hand_interaction) -> `PointerMove`
  datagrams + reliable `Button`/`Scroll` -> per-output wlr virtual pointer (absolute motion).
* Last-used wins: the headset hides its ray cursor when the server reports physical motion.

## Lifecycle
Virtual outputs are removed as soon as the headset disconnects cleanly (app closed). If the connection is lost instead, they persist for a 15 s grace period (`grace_secs`) waiting for a reconnect, then are removed
and Hyprland moves the windows to eDP.

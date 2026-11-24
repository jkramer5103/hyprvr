# Wire protocol (v2)

Source of truth: `crates/proto/src/msg.rs`. Serialization: postcard. Streams carry
`u32 LE length + postcard` frames (`proto::framing`).

* Transport: QUIC on UDP `47800`, ALPN `hyprvr/1`, TLS 1.3, both sides present self-signed
  certificates. Trust = pinned SHA-256 certificate fingerprints (`proto::tls`).
* Discovery: client broadcasts `HYPRVR?1` to UDP `47801`; daemon replies `discovery::Reply`.
* Control stream: the first bidirectional stream, opened by the client.
  `Hello` -> `Welcome{paired}`. If unpaired: SPAKE2 PIN pairing (`proto::pairing`), then
  `State`. An unpaired, unauthenticated client gets nothing but the pairing exchange.
* Screen streams: one server-opened uni stream per screen, starting with `ScreenHeader`,
  then `Tiles` / `VideoStart` / `VideoFrame` / `VideoStop` / `Resized`, applied in order.
  Streams for removed screens are finished; new screens get new streams.
* Environment images: server-opened uni stream with one `EnvironmentImage`.
  `FetchEnvironments` makes the daemon download the recommended free environments
  (`proto::environments`); progress is reported in `State.env_fetch`.
* Datagrams: `Cursor`, `Audio` (S->C), `PointerMove` (C->S).
* Clock: `Ping`/`Pong` gives the client an offset to map `capture_us` (server monotonic
  clock, microseconds) to its own clock for latency stats.

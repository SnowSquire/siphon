# Siphon

Twitch HLS live-stream client: a single-threaded `compio` daemon that follows
channels, records broadcasts and serves them over RTSP, and a ZGUI desktop window
that drives it over an RPC connection.

The development contract and the build, test and lint commands are in
[`AGENTS.md`](AGENTS.md); subsystem design notes are in [`notes/`](notes).

## Windows

The Windows build uses the dynamic MSVC runtime: the UCRT is part of Windows
10/11, and the [Microsoft Visual C++ 2015-2022 Redistributable
(x64)](https://aka.ms/vs/17/release/vc_redist.x64.exe) provides the VCRuntime
and C++ runtime. The installer downloads and installs it when it is missing
(one UAC prompt); the portable zip expects it to be present. Nothing is bundled
with either artifact.

## License

Licensed under either of

- Apache License, Version 2.0, ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license, ([LICENSE-MIT](LICENSE-MIT))

at your option.

### Contribution

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you, as defined in the Apache-2.0 license,
shall be dual licensed as above, without any additional terms or conditions.

### Third party

`crates/daemon/src/rtsp/h264.rs` and `crates/daemon/src/rtsp/aac.rs` are ported
from [mediacommon](https://github.com/bluenviron/mediacommon) and
[gortsplib](https://github.com/bluenviron/gortsplib), both MIT, Copyright (c) 2023
aler9 and Copyright (c) 2020 aler9 respectively.

The window links [libplacebo](https://github.com/haasn/libplacebo)
(LGPL-2.1-or-later) as a shared library; a binary that ships it must keep it
replaceable and carry its license. Archivo and Noto Color Emoji are bundled under
the SIL Open Font License ([`crates/ui/fonts/*-OFL.txt`](crates/ui/fonts)); the
installer copies those texts beside the binaries. Chillax is fetched at build
time under the ITF Free Font License and is not redistributed here.

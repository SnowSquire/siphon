# Siphon

Twitch HLS live-stream client: a single-threaded `compio` daemon that follows
channels, records broadcasts and serves them to its own window over Riffle, and
a ZGUI desktop window that drives it over the same connection.

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

`crates/daemon/src/media/h264.rs` and `crates/daemon/src/media/aac.rs` are ported
from [mediacommon](https://github.com/bluenviron/mediacommon) and
[gortsplib](https://github.com/bluenviron/gortsplib), both MIT, Copyright (c) 2023
aler9 and Copyright (c) 2020 aler9 respectively.

The window links [libplacebo](https://github.com/haasn/libplacebo)
(LGPL-2.1-or-later) into the executable. That is the LGPL's static-link case,
so §6 applies: the recipient has to be able to replace the library and relink.
The sources are here (MIT) with the pinned Nix flake that builds and links them,
so replacing libplacebo and running `nix build .#windows` reproduces the payload
against a modified one. libplacebo in turn links
[shaderc](https://github.com/google/shaderc) as a static `shaderc_combined`
(glslang, SPIRV-Tools and SPIRV-Headers inside it) and vendors fast_float and the
Vulkan headers. Every text ships beside the binaries, and the installer copies
them:

| component | license | text |
|---|---|---|
| libplacebo | LGPL-2.1-or-later | `LICENSE-libplacebo.txt` |
| shaderc | Apache-2.0 | `LICENSE-shaderc.txt` |
| glslang | BSD-style (see the text) | `LICENSE-glslang.txt` |
| SPIRV-Tools | Apache-2.0 | `LICENSE-SPIRV-Tools.txt` |
| SPIRV-Headers | Apache-2.0 / MIT | `LICENSE-SPIRV-Headers.txt` |
| fast_float | Apache-2.0 | `LICENSE-fast_float.txt` |
| Vulkan-Headers | Apache-2.0 | `LICENSE-Vulkan-Headers.txt` |

Archivo is bundled under the SIL Open Font
License ([`crates/ui/fonts/Archivo-OFL.txt`](crates/ui/fonts/Archivo-OFL.txt))
and the installer copies its text beside the binaries; colour emoji come from
the platform's emoji face. Chillax is fetched at build time under the ITF Free
Font License and is not redistributed here.

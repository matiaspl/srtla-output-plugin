# OBS SRTLA Output

Native SRTLA output for OBS Studio on Windows x64.

The plugin keeps the SRT engine and the SRTLA bonding engine in one process. OBS
encoded packets are muxed as MPEG-TS and passed to the patched `irlserver/srt`
library through an in-memory datagram transport; SRTLA then sends the resulting
SRT datagrams over the selected network uplinks.

No localhost UDP proxy is created by the plugin.  The embedded runner now owns
the SRTLA uplink sockets and the patched libsrt transport callbacks in-process;
wire compatibility still requires the Windows/receiver integration tests below.

## Current status

This repository is being built in vertical slices. The current slice contains
the portable adaptive-bitrate controller, bounded engine ABI, OBS output/dock,
Windows adapter discovery, an FFmpeg/libavformat MPEG-TS sink with a bounded
packet/worker path, the embedded
SRTLA runner/SRT session, the `SRT_TRANSPORT_V1` libsrt seam, and the
sender-side forwarding seam shared by UDP and embedded endpoints.

The remaining hardening work is receiver interoperability, Windows route-change
notifications, dedicated-encoder cloning, hardware/link soak tests and
installer QA. MPEG-TS container syntax is delegated to libavformat; the plugin
only owns a custom in-memory AVIO sink and 1316-byte transport packetization.
The output-side ABR tick now applies video bitrate changes when Auto is
selected; no separate proxy process is started.

## Layout

- `plugin/` — OBS output, dock and Windows network monitor.
- `engine/` — Rust FFI facade, queues and adaptive bitrate controller.
- `third_party/irlserver-srt/` — pinned SRT fork with the SRTLA receiver fixes.
- `third_party/srtla_send/` — pinned SRTLA sender implementation and core.

The local libsrt API patch adds `srt_set_external_transport()`. Its callback
contract is deliberately small: `send_datagram` returns zero after accepting a
complete datagram, `receive_datagram` returns zero on timeout and writes the
length to `received`, while `wake` interrupts a pending read and `close` is
called once during teardown.

## Build

The Windows build expects Qt 6, FFmpeg development headers/import libraries,
mbedTLS 3.x development libraries and Rust 1.88 or newer (nightly is required
by the vendored sender's rustfmt configuration). The SRT fork is built with the
same `USE_ENCLIB=mbedtls` provider as the OBS dependency build, with mbedTLS
linked statically; no OpenSSL DLL is required by the plugin. The OBS installer
itself is sufficient at runtime: the
plugin's `OBS::libobs` and `OBS::frontend-api` targets can use generated MSVC
import libraries while loading `obs.dll` and `obs-frontend-api.dll` from the
installed OBS `bin/64bit` directory. No static copy of libobs is embedded.

For the local OBS 32.2.1 installation, the source headers are in
`tools/obs-source`, the generated import libraries are in `tools/obs-dev`, and
the Qt/FFmpeg development prefix is the vcpkg triplet
`tools/obs-ffmpeg-dev` (headers from the exact OBS FFmpeg 8.1.2 source and
import libraries generated from the OBS DLL exports); Qt is installed in
`tools/vcpkg/installed/x64-windows`; install mbedTLS into that triplet before
configuring:

```powershell
tools\vcpkg\vcpkg.exe install mbedtls:x64-windows
```

From a Visual Studio Developer Command
Prompt, configure and build with:

```powershell
cmake -S . -B build-obs -G "Visual Studio 18 2026" -A x64 `
  -DCMAKE_TOOLCHAIN_FILE=tools/vcpkg/scripts/buildsystems/vcpkg.cmake `
  -DOBS_SRTLA_OBS_SOURCE_DIR="$PWD/tools/obs-source" `
  -DOBS_SRTLA_OBS_IMPORT_LIB_DIR="$PWD/tools/obs-dev" `
  -DOBS_SRTLA_OBS_RUNTIME_DIR="C:/Program Files/obs-studio/bin/64bit" `
  -DOBS_SRTLA_FFMPEG_ROOT="$PWD/tools/obs-ffmpeg-dev"
cmake --build build-obs --config Release --target obs-srtla-output
```

If a separate OBS SDK is available, set the three `OBS_SRTLA_OBS_*` paths to
that SDK instead. Linux CI also builds the Rust crates and runs protocol/netem
tests.

To create the distributable ZIP after a Release build, run
`cpack --config build/CPackConfig.cmake -C Release`. Set
`OBS_SRTLA_OBS_PLUGIN_DIR` when the OBS installation uses a non-standard plugin
layout.

## License

The plugin code is GPL-2.0-or-later. Vendored dependencies retain their
upstream licenses; see the individual `LICENSE` files under `third_party/`.

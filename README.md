# SRTLA Output for OBS

Native SRTLA output for OBS Studio on Windows x64, macOS, and Linux.

The plugin muxes OBS-encoded audio and video as MPEG-TS, feeds the SRT packets
directly into an embedded SRTLA sender, and distributes the resulting datagrams
over the network links selected in its OBS dock. The SRT session, SRTLA sender,
and uplink sockets all run in the OBS process; no localhost UDP proxy or helper
process is required.

An SRTLA-compatible receiver is required at the destination. This repository
does **not** ship a production receiver. For a local Windows smoke test, see
[`docs/SRTLA-LOCAL-RELAY.md`](docs/SRTLA-LOCAL-RELAY.md).

## Features

- An OBS dock for starting and stopping the output, selecting uplinks, and
  monitoring the session.
- Streaming or recording encoder reuse, plus independent custom encoders.
- H.264 and HEVC video with AAC or Opus audio.
- BELABOX-style automatic video bitrate control driven by end-to-end SRT RTT
  and instantaneous sender-queue depth. Manual bitrate and maximum-bitrate
  controls remain available.
- Per-adapter IPv4 and IPv6 uplinks that can be enabled or disabled while the
  output is running. Newly discovered links are opt-in, and the last enabled
  link cannot be disabled.
- ABR state and thresholds, end-to-end SRT RTT and sender-queue pressure,
  offered and ACK-delivered traffic, current/target video bitrate, raw SRT
  bandwidth and per-link CC telemetry, plus per-link state, NAK-recency score,
  RTT, retransmission-request rate, congestion, and scheduler diagnostics.
- SRT stream ID and optional passphrase authentication. Passphrases are
  confidential configuration and are stored plainly in the OBS profile; they
  are never written to logs or back into the SRTLA URL.
- Automatic SRT reconnection and a bounded, keyframe-aware media queue. After a
  reconnect, stale media is discarded and MPEG-TS resumes with fresh tables at
  the next keyframe.
- Profile-scoped settings and safe teardown when the OBS profile changes.

Automatic bitrate and live manual-bitrate changes require an OBS encoder that
advertises dynamic bitrate support. With a compatible encoder, automatic mode
can be toggled and the manual bitrate adjusted while the output is live. Other
encoders remain usable at a fixed bitrate while the dock continues to show
transport telemetry.

## Current status and limitations

This is an early `0.1.2` development release, not a production-certified
release. The in-process SRT/SRTLA path, OBS output and dock,
adaptive-bitrate controller, platform-native adapter discovery, MPEG-TS muxer,
reconnect path, and bounded engine ABI are implemented.

Current operational limitations:

- Streaming and recording encoders can be reused only while their source OBS
  output is stopped. Starting another OBS output that needs the same encoder
  stops the SRTLA output to avoid conflicting capture ownership.
- The Custom encoder path creates a dedicated encoder from that encoder's
  defaults and applies the selected bitrate. It does not clone all settings
  from the streaming or recording encoder.
- Network adapters are refreshed by polling once per second; native route-
  change notifications are not implemented yet.
- Receiver interoperability has a local Windows relay smoke-test path, but no
  maintained production-receiver compatibility matrix or hardware/link soak
  coverage yet.
- Packaging produces manual-install archives for Windows, macOS, and Linux.
  Upgrade and release-package QA remain to be completed.

## TODO

- Validate and document interoperability with maintained production SRTLA
  receivers.
- Provision the receiver, SRT tools, privileges, and kernel support needed to
  run the existing network-namespace/netem scenarios in project CI. They are
  currently discovered by the sender workspace test command but skip when
  those dependencies are unavailable.
- Replace or augment one-second adapter polling with Windows route-change
  notifications.
- Clone streaming/recording encoder settings into an independent dedicated
  encoder.
- Run extended hardware-encoder, multi-link, impairment, reconnect, and soak
  testing.
- Complete upgrade and release-package QA.

## Install

The release packages contain an unsigned macOS installation helper rather than
a signed native installer. Close OBS before installing a package.

### Windows

Extract the ZIP into the OBS installation directory, preserving the
`obs-plugins/64bit` and `data/obs-plugins` paths. The default directory is
`C:\Program Files\obs-studio`.

### macOS

The macOS release ZIP includes `install-macos.command`. On Apple Silicon
(M1/M2/M3/M4), download the `macos-arm64` package. Then:

1. Open the downloaded ZIP and open the extracted folder.
2. Hold Control and click (or right-click) `install-macos.command`, then choose
   **Open**.
3. In the macOS warning, choose **Open** again. This one-time warning is
   expected because the release package is not notarized.
4. Wait for the Terminal window to report that installation completed, press
   Return to close it, and start OBS.

If macOS still blocks the helper, open **System Settings > Privacy & Security**
and choose **Open Anyway** for `install-macos.command`, then repeat step 2.

The helper copies `srtla-output.plugin` into:

```text
~/Library/Application Support/obs-studio/plugins/
```

It removes the quarantine flag and applies a local ad-hoc signature. If an
older plugin is already installed, it is moved aside as a backup before the
new one is copied.

Manual fallback for technical users:

```sh
plugin="$HOME/Library/Application Support/obs-studio/plugins/srtla-output.plugin"
xattr -dr com.apple.quarantine "$plugin"
codesign --force --deep --sign - "$plugin"
```

### Linux

Extract the `tar.gz` into the same prefix used by OBS. The package follows the
standard OBS layout and contains `lib*/obs-plugins/srtla-output.so` and
`share/obs/obs-plugins/srtla-output/`. Distribution packages may use a
different `lib` directory; set `OBS_SRTLA_OBS_PLUGIN_DIR` when building for
that prefix.

Start OBS and open **Docks > SRTLA Output**.

## Use

1. Enter the receiver endpoint as `srtla://host:port`. Keep credentials out of
   the URL; legacy `streamid`, `passphrase`, and `password` query parameters are
   migrated to separate profile fields.
2. Enter the stream ID expected by the receiver and, if encryption is required,
   a 10–79-byte UTF-8 passphrase. The passphrase is stored unencrypted in the
   OBS profile on every supported platform. Profiles created by the removed
   DPAPI implementation require entering the passphrase again.
3. Choose **Streaming** or **Recording** to reuse that stopped OBS output's
   encoders, or **Custom** to create independent video and audio encoders.
4. Select at least one operational network link.
5. Choose automatic bitrate with a maximum, or disable it and set a manual
   bitrate.
6. Select **Start**. `Live` requires both an established SRT session and at
   least one payload-eligible SRTLA link.

The dock reports `Starting`, `Live`, `Waiting for network`, `Reconnecting`, or
`Error`. `Reconnecting` is recoverable; `Error` indicates a local fatal failure
such as invalid socket, engine, encoder, muxer, or queue state.

## Automatic bitrate control

The automatic controller ports the closed-loop congestion response from
[BELABOX `belacoder`](https://github.com/BELABOX/belacoder/blob/master/belacoder.c)
to Rust. While the output is active, it samples the SRT socket every 20 ms and
observes:

- the instantaneous number of unacknowledged packets in the SRT sender buffer;
- end-to-end smoothed SRT RTT and the negotiated SRT latency;
- the current SRT send rate, used only to express part of the queue threshold
  in packets.

The controller learns moving queue and RTT baselines, positive jitter, RTT
trend, and a slowly adapting minimum RTT. It does not use the SRT packet-pair
bandwidth estimate or the sum of per-link SRTLA CC targets to select the encoder
bitrate. Those values remain visible as diagnostics. SRTLA CC controls packet
scheduling across links; end-to-end queue growth and RTT inflation determine
whether the offered encoder rate is sustainable.

Bitrate changes follow BELABOX's asymmetric control law:

- If RTT reaches one third of negotiated SRT latency, or queue growth crosses
  the severe adaptive threshold, video immediately drops to the configured
  minimum.
- Under heavy congestion (RTT above one fifth of latency or a high queue), the
  controller subtracts 100 kb/s plus 10% of its internal target, no more than
  once every 250 ms.
- Under light congestion (RTT or queue above its learned baseline), it
  subtracts 100 kb/s, no more than once every 200 ms.
- With RTT close to the learned minimum and no upward RTT trend, it adds
  30 kb/s plus one thirtieth of the internal target, no more than once every
  500 ms.
- The internal target is clamped to the configured minimum and maximum. Values
  applied to OBS are rounded down to 100 kb/s, while sub-step changes continue
  accumulating internally.

Automatic mode starts at the configured maximum, matching BELABOX. The default
automatic maximum is 6000 kb/s and its range is capped at 30000 kb/s; the old
100000 kb/s sentinel is migrated to the new default. This ceiling does not
limit the separate manual-bitrate setting. If the SRT session or
every enabled link is down, the output falls to the minimum to avoid an
unbounded queue. Missing or repeated statistics hold the current target rather
than replaying a congestion event. Learned queue and RTT state is reset after a
session transition.

The per-link **NAK score** and **NAK rate** remain SRTLA steering signals, not
end-to-end unrecovered packet loss. A NAK is a retransmission request, and the
rate includes packets that SRT later repairs within its latency window. It is
therefore expected that `srt-live-transmit` can report no final loss while the
dock shows a non-zero NAK rate and a temporarily reduced NAK score.

Turning automatic mode off stops bitrate probing and applies the manual value.
Live automatic/manual switching and live manual changes require an encoder that
advertises OBS dynamic-bitrate support. The automatic maximum can also be
changed while live; lowering it caps a compatible encoder immediately, while
raising it lets the feedback loop probe upward normally.

## Architecture

- `plugin/` — OBS output, dock, platform network monitors, SRT session, and
  libavformat MPEG-TS sink.
- `engine/` — Rust FFI facade, bounded queues, statistics, and adaptive-bitrate
  controller.
- `third_party/irlserver-srt/` — pinned SRT fork with the external-transport
  patch.
- `third_party/srtla_send/` — pinned SRTLA sender implementation and core.

MPEG-TS container syntax is delegated to libavformat. The plugin owns the
custom in-memory AVIO sink and 1316-byte transport packetization. The local
libsrt patch adds `srt_set_external_transport()`, whose versioned callback
contract connects libsrt directly to the bounded engine queues.

## Build

### Requirements

- Windows x64 with an x64 MSVC developer environment;
- macOS 12 or newer with Xcode command-line tools;
- Linux x86_64 or arm64 with a C++17 compiler, Ninja, and pkg-config;
- CMake 3.28 or newer, Git, and stable Rust 1.88 or newer.

The vendored sender has a rustfmt configuration that uses unstable formatting
options. Nightly Rust is needed only to format that vendored workspace, not to
build the plugin or run the project CI configuration.

The pinned provisioning script downloads the OBS 32.2.1 runtime and source,
the matching 2026-07-15 Qt/FFmpeg/mbedTLS dependency bundles, verifies their
archive hashes, and creates the required OBS import libraries. Run it from an
x64 Visual Studio Developer PowerShell:

```powershell
.\scripts\prepare-obs-ci.ps1

Get-Content .ci-deps\paths.env | ForEach-Object {
  $name, $value = $_ -split '=', 2
  Set-Item -Path "Env:$name" -Value $value
}

cmake -S . -B build-obs -G Ninja `
  -DCMAKE_BUILD_TYPE=Release `
  -DOBS_SRTLA_BUILD_TESTS=ON `
  -DOBS_SRTLA_BUILD_VENDOR_SRT=ON `
  -DOBS_SRTLA_REQUIRE_PLUGIN=ON `
  -DCMAKE_PREFIX_PATH="$env:CMAKE_PREFIX_PATH" `
  -DOBS_SRTLA_OBS_SOURCE_DIR="$env:OBS_SRTLA_OBS_SOURCE_DIR" `
  -DOBS_SRTLA_OBS_IMPORT_LIB_DIR="$env:OBS_SRTLA_OBS_IMPORT_LIB_DIR" `
  -DOBS_SRTLA_OBS_RUNTIME_DIR="$env:OBS_SRTLA_OBS_RUNTIME_DIR" `
  -DOBS_SRTLA_FFMPEG_ROOT="$env:OBS_SRTLA_FFMPEG_ROOT" `
  -DOBS_SRTLA_MBEDTLS_ROOT="$env:OBS_SRTLA_MBEDTLS_ROOT"

cmake --build build-obs --config Release --target srtla-output --parallel
ctest --test-dir build-obs -C Release --output-on-failure
cpack --config build-obs\CPackConfig.cmake -C Release
```

The SRT fork and mbedTLS are linked statically into the plugin, so no OpenSSL or
mbedTLS runtime library is shipped. Qt, FFmpeg, and OBS are provided by the
host installation; no static copy of libobs is embedded.

If you already have a compatible OBS SDK and dependency set, provide
`OBS_SRTLA_OBS_SOURCE_DIR`, `OBS_SRTLA_OBS_IMPORT_LIB_DIR`,
`OBS_SRTLA_OBS_RUNTIME_DIR`, `OBS_SRTLA_FFMPEG_ROOT`,
`OBS_SRTLA_MBEDTLS_ROOT`, and the Qt prefix directly instead of running the
provisioning script.

For macOS and Linux, provide an OBS 32 development prefix containing the
`libobs` and `obs-frontend-api` CMake packages, Qt6 Widgets, FFmpeg headers and
libraries, and mbedTLS. The same cache variables used by the Windows build can
be supplied directly:

```sh
cmake -S . -B build \
  -G Ninja \
  -DCMAKE_BUILD_TYPE=Release \
  -DOBS_SRTLA_BUILD_TESTS=ON \
  -DOBS_SRTLA_BUILD_VENDOR_SRT=ON \
  -DOBS_SRTLA_REQUIRE_PLUGIN=ON \
  -DCMAKE_PREFIX_PATH="$OBS_SRTLA_OBS_PREFIX" \
  -DOBS_SRTLA_FFMPEG_ROOT="$OBS_SRTLA_FFMPEG_ROOT" \
  -DOBS_SRTLA_MBEDTLS_ROOT="$OBS_SRTLA_MBEDTLS_ROOT"

cmake --build build --parallel
ctest --test-dir build --output-on-failure
cpack --config build/CPackConfig.cmake
```

On macOS, configure one architecture at a time:

```sh
cmake -S . -B build-macos-arm64 -G Xcode \
  -DCMAKE_OSX_ARCHITECTURES=arm64 \
  -DCMAKE_OSX_DEPLOYMENT_TARGET=12.0 \
  -DOBS_SRTLA_REQUIRE_PLUGIN=ON
cmake --build build-macos-arm64 --config Release --parallel
ctest --test-dir build-macos-arm64 -C Release --output-on-failure
cpack --config build-macos-arm64/CPackConfig.cmake -C Release
```

On Linux, use `aarch64`/`arm64` or `x86_64` natively and set
`CMAKE_INSTALL_LIBDIR` when the OBS installation uses a multiarch library
directory, for example `lib/x86_64-linux-gnu`.

The CI matrix builds the plugin natively for Windows x64, Linux x86_64/arm64,
and macOS x86_64/arm64, runs the Rust and CTest suites, validates archive
contents, and uploads one package per architecture. A semantic-version tag
matching the CMake project version publishes all packages and
`SHA256SUMS.txt` as a GitHub Release.

Pushing a plain semantic-version tag that matches the CMake project version
(for example, `0.1.2`) runs the same checks. Only after both jobs pass, GitHub
Actions publishes the five platform/architecture archives and
`SHA256SUMS.txt` as a GitHub Release. Re-running the tagged workflow safely
replaces the release assets. See [TODO](#todo) for the network-namespace/netem
coverage gap.

## License

The plugin code is GPL-2.0-or-later. Vendored dependencies retain their
upstream licenses; see the individual `LICENSE` files under `third_party/`.

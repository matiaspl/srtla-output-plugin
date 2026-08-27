# OBS SRTLA Output

Native SRTLA output for OBS Studio on Windows x64.

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
- Automatic video bitrate control based on the lower of the eligible-link CC
  aggregate and a validated end-to-end SRT bandwidth estimate. SRT sender-queue
  pressure can trigger an immediate hysteretic reduction; manual bitrate and
  maximum-bitrate controls remain available.
- Per-adapter IPv4 and IPv6 uplinks that can be enabled or disabled while the
  output is running. Newly discovered links are opt-in, and the last enabled
  link cannot be disabled.
- Effective, link-aggregate, and SRT capacity telemetry, SRT sender-queue
  pressure, offered and ACK-delivered traffic, current/recommended video
  bitrate, plus per-link state, NAK-recency score, RTT, retransmission-request
  rate, congestion, and scheduler diagnostics.
- SRT stream ID and optional passphrase authentication. Saved passphrases are
  protected with Windows DPAPI instead of being stored in plaintext.
- Automatic SRT reconnection and a bounded, keyframe-aware media queue. After a
  reconnect, stale media is discarded and MPEG-TS resumes with fresh tables at
  the next keyframe.
- Profile-scoped settings and safe teardown when the OBS profile changes.

Automatic bitrate and live manual-bitrate changes require an OBS encoder that
advertises dynamic bitrate support. With a compatible encoder, automatic mode
can be toggled and the manual bitrate adjusted while the output is live. Other
encoders remain usable at a fixed bitrate while the dock continues to show the
recommended bitrate as telemetry.

## Current status and limitations

This is an early `0.1.0` development slice, not a production release. The
in-process SRT/SRTLA path, OBS output and dock, adaptive-bitrate controller,
Windows adapter discovery, MPEG-TS muxer, reconnect path, and bounded engine
ABI are implemented.

Current operational limitations:

- Streaming and recording encoders can be reused only while their source OBS
  output is stopped. Starting another OBS output that needs the same encoder
  stops the SRTLA output to avoid conflicting capture ownership.
- The Custom encoder path creates a dedicated encoder from that encoder's
  defaults and applies the selected bitrate. It does not clone all settings
  from the streaming or recording encoder.
- Network adapters are refreshed by polling once per second; native Windows
  route-change notifications are not implemented yet.
- Receiver interoperability has a local Windows relay smoke-test path, but no
  maintained production-receiver compatibility matrix or hardware/link soak
  coverage yet.
- Packaging produces a manual-install ZIP. Installer and upgrade QA remain to
  be completed.

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
- Complete installer, upgrade, and release-package QA.

## Install

There is no installer yet. After creating the ZIP described in [Build](#build):

1. Close OBS.
2. Open the generated ZIP and copy the contents of its top-level directory into
   the OBS installation directory, preserving the `obs-plugins/64bit` and
   `data/obs-plugins` paths. The default installation directory is
   `C:\Program Files\obs-studio`.
3. Start OBS and open **Docks > SRTLA Output**.

## Use

1. Enter the receiver endpoint as `srtla://host:port`. Keep credentials out of
   the URL; legacy `streamid`, `passphrase`, and `password` query parameters are
   migrated to separate profile fields.
2. Enter the stream ID expected by the receiver and, if encryption is required,
   a 10–79-byte UTF-8 passphrase.
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

The automatic bitrate controller runs once per second while the output is
active. It starts from the selected encoder's actual bitrate, treats the
configured maximum as a ceiling, reserves the configured audio bitrate, and
uses an 80% capacity safety margin by default. The minimum video bitrate is
500 kb/s by default.

The controller combines three kinds of evidence:

- **Per-link capacity:** each enabled, payload-eligible link has its own
  congestion-control target. Only measured targets are summed; bootstrap and
  initial placeholder targets are not considered capacity-ready. A link becomes
  ready only after a complete offered-rate window exists and its congestion
  control has left bootstrap.
- **Proven traffic:** per-link SRTLA acknowledgements provide the rate already
  delivered successfully. When the SRT sender is healthy, the current video
  plus audio rate is also treated as proven load. This lower bound prevents a
  working high-rate stream from being reduced by a startup placeholder or a
  bandwidth estimate that merely follows the offered load.
- **End-to-end SRT state:** libsrt supplies its bandwidth estimate, sender
  buffer delay, retransmitted bytes, and dropped bytes. A bandwidth estimate
  must be non-zero and appear in three distinct fresh samples before it can cap
  the link aggregate. Decreases are filtered faster than increases. Without
  independent congestion evidence, an estimate is rejected if applying the
  safety margin would place it below traffic that is already working.

In steady state, the calculation is conceptually:

```text
measured link capacity = sum(ready eligible link CC targets)
proven capacity floor  = proven media load / safety margin
effective link capacity = max(measured link capacity, proven capacity floor)
effective capacity = min(effective link capacity, validated SRT capacity)
recommended video = clamp(effective capacity * safety margin - audio,
                          minimum video, maximum video)
```

If no validated SRT estimate is available, effective link capacity is used on
its own. The dock therefore shows separate **Link**, **SRT**, and **Effective**
capacity values. **Recommended** is the controller's full target; **Current**
is the encoder bitrate after the controller's stability and step limits.

The per-link **NAK score** and **NAK rate** are SRTLA steering signals, not
end-to-end unrecovered packet loss. A NAK is a retransmission request, and the
rate includes packets that SRT later repairs within its latency window. It is
therefore expected that `srt-live-transmit` can report no final loss while the
dock shows a non-zero NAK rate and a temporarily reduced NAK score.

Bitrate changes use asymmetric hysteresis:

- A recommendation more than 10% below the current bitrate is applied
  immediately.
- An increase requires at least 15% headroom for 10 consecutive controller
  ticks. The first increase is therefore delayed by roughly 10 seconds.
- Increases are limited to 10% of the current bitrate, with a 50 kb/s minimum
  step and a 500 kb/s maximum step. Further increases occur no more than once
  every five ticks while headroom remains stable.
- Applied values are rounded down to 50 kb/s and clamped to the configured
  minimum and maximum.
- Adding or re-enabling a link freezes increases for 10 ticks while it warms
  up, but does not suppress evidence-based reductions.

Sender pressure provides a separate emergency path. The transport is stressed
when the SRT send buffer reaches 250 ms, when SRT reports dropped bytes, or when
the buffer is above 50 ms while retransmissions reach 20%. A newly detected
stress event immediately caps the next target at 80% of the current bitrate,
and growth is frozen while stress persists. Another emergency reduction is
armed only after the buffer returns to at most 50 ms, drops are zero, and
retransmissions are at most 10%; this prevents a backed-up sender from
repeatedly cutting the bitrate on every tick.

During initial measurement, automatic mode keeps the greater of the actual
encoder bitrate and the configured start bitrate instead of reducing from an
unmeasured value. If the SRT session or every enabled link is down, it switches
to the minimum bitrate to keep the output alive without growing an unbounded
queue.

Turning automatic mode off does not stop the calculation: the dock continues
to display the recommendation as telemetry, while the manual value is applied
to the encoder. Live automatic/manual switching and live manual changes require
an encoder that advertises OBS dynamic-bitrate support. The maximum can also be
changed while live; lowering it caps a compatible encoder immediately, while
raising it lets the normal stability rules govern subsequent growth.

## Architecture

- `plugin/` — OBS output, dock, Windows network monitor, SRT session, and
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

- Windows x64 and an x64 MSVC developer environment
- CMake 3.28 or newer and Ninja
- Git and PowerShell
- Stable Rust 1.88 or newer

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

cmake --build build-obs --config Release --target obs-srtla-output --parallel
ctest --test-dir build-obs -C Release --output-on-failure
cpack --config build-obs\CPackConfig.cmake -C Release
```

The SRT fork and mbedTLS are linked statically into the plugin, so no OpenSSL or
mbedTLS DLL is shipped. Qt, FFmpeg, `obs.dll`, and `obs-frontend-api.dll` are
resolved from the OBS runtime; no static copy of libobs is embedded.

If you already have a compatible OBS SDK and dependency set, provide
`OBS_SRTLA_OBS_SOURCE_DIR`, `OBS_SRTLA_OBS_IMPORT_LIB_DIR`,
`OBS_SRTLA_OBS_RUNTIME_DIR`, `OBS_SRTLA_FFMPEG_ROOT`,
`OBS_SRTLA_MBEDTLS_ROOT`, and the Qt prefix directly instead of running the
provisioning script.

Windows CI builds the native plugin and runs the Rust engine, transport ABI,
output lifecycle, and secret-store tests. Linux CI runs the engine and vendored
sender workspaces' unit, protocol, and dependency-independent integration
tests. The Windows job uses the OBS 32 development environment, produces an
OBS-ready ZIP, and uploads it as a workflow artifact.

Pushing a plain semantic-version tag that matches the CMake project version
(for example, `0.1.0`) runs the same checks. Only after both jobs pass, GitHub
Actions publishes the ZIP and its `SHA256SUMS.txt` as a GitHub Release. Re-running
the tagged workflow safely replaces the release assets. See [TODO](#todo) for
the network-namespace/netem coverage gap.

## License

The plugin code is GPL-2.0-or-later. Vendored dependencies retain their
upstream licenses; see the individual `LICENSE` files under `third_party/`.

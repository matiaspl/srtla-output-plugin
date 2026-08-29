# Local SRTLA relay (Windows)

This repository ships the SRTLA output for OBS and the Rust sender/transport seam; it does **not** ship a production SRTLA receiver. For local testing, use the Windows-compatible `srtla_rec` build from [manueldev/srtla-windows](https://github.com/manueldev/srtla-windows), plus `srt-live-transmit` and FFmpeg.

## Topology

```text
SRTLA output for OBS
  srtla://127.0.0.1:5000
        | UDP SRTLA
        v
srtla_rec :5000  ->  SRT listener :5002
                         |
                         v
                 srt-live-transmit
                 listener :5001
                         |
                         v
                    OBS/ffplay/ffmpeg
```

`srtla_rec` rebuilds the SRT packet stream from the SRTLA links. It does not decode media. The downstream SRT listener (or an SRT-aware player) is what verifies the MPEG-TS stream.

## Start the local relay

Build `srtla_rec.exe` from the receiver repository and place it in
`.relay-src` under this repository. Put `srt-live-transmit.exe` and
`ffmpeg.exe` on `PATH`, or replace the `Get-Command` expressions below with
their full paths.

Run the following commands from this repository's root. Start the receiver
first and retain its process object for precise cleanup:

```powershell
$receiverExe = (Resolve-Path .\.relay-src\srtla_rec.exe).Path
$receiverProcess = Start-Process $receiverExe `
  -ArgumentList '5000','127.0.0.1','5002','--log-errors' `
  -RedirectStandardOutput .\.relay-src\srtla_rec.stdout.log `
  -RedirectStandardError .\.relay-src\srtla_rec.stderr.log `
  -PassThru
```

Then start the SRT bridge. The `lossmaxttl` and `latency` options allow the receiver to reorder bonded packets:

```powershell
$srtLiveTransmit = (Get-Command srt-live-transmit.exe -ErrorAction Stop).Source
$bridgeProcess = Start-Process $srtLiveTransmit `
  -ArgumentList 'srt://127.0.0.1:5002?mode=listener&lossmaxttl=40&latency=2000', `
              'srt://0.0.0.0:5001?mode=listener' `
  -RedirectStandardOutput .\srt-live-transmit.stdout.log `
  -RedirectStandardError .\srt-live-transmit.stderr.log `
  -PassThru
```

Expected receiver log:

```text
Trying to connect to SRT at 127.0.0.1:5002... success
srtla_rec is now running
```

Check listeners without stopping unrelated processes:

```powershell
Get-NetUDPEndpoint -LocalPort 5000,5001,5002
```

Stop only the two processes started above when finished:

```powershell
Stop-Process -Id $receiverProcess.Id,$bridgeProcess.Id
```

## Configure the OBS output for this relay

In the SRTLA dock set:

- **SRTLA URL:** `srtla://127.0.0.1:5000`
- **Stream ID:** `obs-local-smoke-test`
- **Passphrase:** leave empty for an explicit unauthenticated local test, or
  use 10–79 UTF-8 bytes. The plugin stores this confidential configuration
  plainly in the OBS profile and never writes it to logs or the URL.
- Select an available video/audio encoder and at least one operational network link.

The URL is endpoint-only. Do not put `streamid`, `passphrase`, or `password` query parameters in it. Legacy query parameters are accepted once and migrated to separate profile fields.

## Verify media traversed the relay

Use an SRT caller to consume the bridge listener. FFmpeg can validate packets without opening a preview window:

```powershell
$ffmpeg = (Get-Command ffmpeg.exe -ErrorAction Stop).Source
& $ffmpeg -hide_banner -loglevel info `
  -i 'srt://127.0.0.1:5001?mode=caller&latency=2000' `
  -t 10 -map 0 -f null NUL
```

A successful smoke test reports an input stream and packet/frame counters. If the receiver is running but no SRTLA link registers, the OBS dock remains in `Waiting for network`/`Reconnecting`; that is not a media-path success.

## Operation and troubleshooting

- `Starting` means OBS is preparing encoders and waiting for the initial SRT connection (up to five seconds).
- `Live` requires both an established SRT session and at least one eligible SRTLA link.
- `Reconnecting` is an ordinary network interruption. Capture stays active, stale media is dropped, and output resumes on a fresh keyframe.
- `Error` is reserved for local fatal failures (invalid socket setup, dead engine, encoder/muxer failure, or an unrecoverable queue error).
- If port 5000 is occupied, choose another UDP port for both the dock URL and `srtla_rec`.
- If the bridge cannot bind 5002, stop the old `srt-live-transmit` instance or select a free SRT port in both commands.
- The receiver fork is experimental and not a production relay. For internet-facing use, run a maintained server such as IRLServer/OpenIRL behind a firewall and publish only the required UDP ports.

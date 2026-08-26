param(
    [string]$Root = (Resolve-Path (Join-Path $PSScriptRoot '..'))
)

$ErrorActionPreference = 'Stop'

# OBS 32.2.1 was released with the 2026-07-15 dependency bundle.  Keep the
# artifact names and hashes here so a CI run cannot silently move to a newer
# Qt/FFmpeg/mbedTLS ABI.
$obsVersion = '32.2.1'
$depsVersion = '2026-07-15'
$artifacts = @{
    "windows-deps-$depsVersion-x64.zip" = '6f90e9598fa10cff5ad23cdcfae49b87868c07bf896b02cd464582b4ce2f2ba9'
    "windows-deps-qt6-$depsVersion-x64.zip" = '7c7f985711d80467bdc1795b6592275a27d5b0e5a2c7a61db1f2c1d08d6a5579'
    "OBS-Studio-$obsVersion-Windows-x64.zip" = 'db64a2934f8261f85b1410b84be011207a0afda5400d008289f1f1e211bcc7de'
}

$cache = Join-Path $Root '.ci-cache'
$prefix = Join-Path $Root '.ci-deps'
$null = New-Item -ItemType Directory -Force -Path $cache, $prefix

function Get-PinnedArtifact([string]$Name, [string]$Sha256, [string]$Url) {
    $path = Join-Path $cache $Name
    if (!(Test-Path -LiteralPath $path)) {
        Invoke-WebRequest -Uri $Url -OutFile $path
    }
    $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $path).Hash.ToLowerInvariant()
    if ($actual -ne $Sha256) {
        throw "Checksum mismatch for $Name (expected $Sha256, got $actual)"
    }
    return $path
}

function Expand-Pinned([string]$Archive, [string]$Destination) {
    if (Test-Path -LiteralPath $Destination) {
        return
    }
    $staging = Join-Path $cache ([IO.Path]::GetFileNameWithoutExtension($Archive))
    if (!(Test-Path -LiteralPath $staging)) {
        Expand-Archive -LiteralPath $Archive -DestinationPath $staging
    }
    $children = @(Get-ChildItem -LiteralPath $staging)
    if ($children.Count -eq 1 -and $children[0].PSIsContainer) {
        Move-Item -LiteralPath $children[0].FullName -Destination $Destination
    } else {
        Move-Item -LiteralPath $staging -Destination $Destination
    }
}

$depsUrl = "https://github.com/obsproject/obs-deps/releases/download/$depsVersion/"
$runtimeName = "OBS-Studio-$obsVersion-Windows-x64.zip"
$runtimeArchive = Get-PinnedArtifact $runtimeName $artifacts[$runtimeName] `
    "https://github.com/obsproject/obs-studio/releases/download/$obsVersion/$runtimeName"
$runtimeRoot = Join-Path $prefix 'obs-runtime'
Expand-Pinned $runtimeArchive $runtimeRoot

$ffmpegName = "windows-deps-$depsVersion-x64.zip"
$ffmpegArchive = Get-PinnedArtifact $ffmpegName $artifacts[$ffmpegName] ($depsUrl + $ffmpegName)
$ffmpegRoot = Join-Path $prefix 'obs-ffmpeg-dev'
Expand-Pinned $ffmpegArchive $ffmpegRoot

$qtName = "windows-deps-qt6-$depsVersion-x64.zip"
$qtArchive = Get-PinnedArtifact $qtName $artifacts[$qtName] ($depsUrl + $qtName)
$qtRoot = Join-Path $prefix 'qt6'
Expand-Pinned $qtArchive $qtRoot

$obsSource = Join-Path $prefix 'obs-source'
if (!(Test-Path -LiteralPath (Join-Path $obsSource 'libobs/obs.h'))) {
    git clone --depth 1 --branch $obsVersion --recursive https://github.com/obsproject/obs-studio.git $obsSource
}

$obsBin = Get-ChildItem -LiteralPath $runtimeRoot -Recurse -Filter 'obs.dll' | Select-Object -First 1
$frontendBin = Get-ChildItem -LiteralPath $runtimeRoot -Recurse -Filter 'obs-frontend-api.dll' | Select-Object -First 1
if (!$obsBin -or !$frontendBin) {
    throw 'The pinned OBS runtime archive did not contain obs.dll and obs-frontend-api.dll'
}

$dumpbin = Get-Command dumpbin.exe -ErrorAction SilentlyContinue
$lib = Get-Command lib.exe -ErrorAction SilentlyContinue
if (!$dumpbin -or !$lib) {
    throw 'Run this script from an MSVC developer environment (dumpbin.exe and lib.exe are required)'
}

function New-ImportLibrary([IO.FileInfo]$Dll, [string]$Name, [string]$OutputDirectory) {
    $def = Join-Path $OutputDirectory "$Name.def"
    $lines = @("LIBRARY $($Dll.Name)", 'EXPORTS')
    (& $dumpbin.Source /exports $Dll.FullName) | ForEach-Object {
        if ($_ -match '^\s*\d+\s+[0-9A-F]+\s+[0-9A-F]+\s+(\S+)\s*$') {
            $lines += "  $($Matches[1])"
        }
    }
    if ($lines.Count -le 2) {
        throw "No exports found in $($Dll.FullName)"
    }
    Set-Content -LiteralPath $def -Value $lines -Encoding ascii
    & $lib.Source /nologo /machine:x64 "/def:$def" "/out:$(Join-Path $OutputDirectory "$Name.lib")"
    if ($LASTEXITCODE -ne 0) { throw "Failed to create $Name.lib" }
}

$obsDev = Join-Path $prefix 'obs-dev'
$null = New-Item -ItemType Directory -Force -Path $obsDev
New-ImportLibrary $obsBin 'obs' $obsDev
New-ImportLibrary $frontendBin 'obs-frontend-api' $obsDev

$mbedtlsHeader = Get-ChildItem -LiteralPath $prefix -Recurse -Filter 'aes.h' |
    Where-Object { $_.FullName -match 'mbedtls' } | Select-Object -First 1
if (!$mbedtlsHeader) {
    throw 'The pinned OBS dependency bundle did not provide mbedTLS 3 headers/libraries'
}
$mbedtlsRoot = $mbedtlsHeader.Directory.Parent.Parent.FullName
$mbedtlsVersion = Get-ChildItem -LiteralPath $mbedtlsRoot -Recurse -Filter 'version.h' |
    Select-Object -First 1
if (!$mbedtlsVersion -or !(Get-Content $mbedtlsVersion.FullName -Raw | Select-String 'MBEDTLS_VERSION_MAJOR\s+3')) {
    throw 'The pinned OBS dependency bundle is not using mbedTLS 3'
}

$ffmpegVersion = Get-ChildItem -LiteralPath $ffmpegRoot -Recurse -Filter 'version.h' |
    Where-Object { $_.FullName -match 'libavutil' } | Select-Object -First 1
if (!$ffmpegVersion) {
    throw 'The pinned OBS dependency bundle did not provide FFmpeg development headers'
}
$ffmpegText = Get-Content $ffmpegVersion.FullName -Raw
if ($ffmpegText -notmatch 'LIBAVUTIL_VERSION_MAJOR\s+60' -or
    $ffmpegText -notmatch 'LIBAVUTIL_VERSION_MINOR\s+26') {
    throw 'The pinned OBS dependency bundle is not FFmpeg 8.1.2 (libavutil 60.26)'
}

"OBS_SRTLA_OBS_SOURCE_DIR=$obsSource" | Out-File (Join-Path $prefix 'paths.env') -Encoding utf8
"OBS_SRTLA_OBS_IMPORT_LIB_DIR=$obsDev" | Out-File (Join-Path $prefix 'paths.env') -Append -Encoding utf8
"OBS_SRTLA_OBS_RUNTIME_DIR=$($obsBin.Directory.FullName)" | Out-File (Join-Path $prefix 'paths.env') -Append -Encoding utf8
"OBS_SRTLA_FFMPEG_ROOT=$ffmpegRoot" | Out-File (Join-Path $prefix 'paths.env') -Append -Encoding utf8
"OBS_SRTLA_MBEDTLS_ROOT=$mbedtlsRoot" | Out-File (Join-Path $prefix 'paths.env') -Append -Encoding utf8
"CMAKE_PREFIX_PATH=$qtRoot" | Out-File (Join-Path $prefix 'paths.env') -Append -Encoding utf8

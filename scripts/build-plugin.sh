#!/usr/bin/env sh
set -eu

ROOT_DIR=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
BUILD_DIR=${BUILD_DIR:-"$ROOT_DIR/build"}
BUILD_TYPE=${BUILD_TYPE:-Release}

set -- \
  -S "$ROOT_DIR" \
  -B "$BUILD_DIR" \
  -D"CMAKE_BUILD_TYPE=$BUILD_TYPE" \
  -DOBS_SRTLA_BUILD_TESTS=ON \
  -DOBS_SRTLA_BUILD_VENDOR_SRT=ON \
  -DOBS_SRTLA_REQUIRE_PLUGIN=ON

if [ "$(uname -s)" = "Darwin" ]; then
  set -- "$@" \
    -G Xcode \
    -D"CMAKE_OSX_ARCHITECTURES=${CMAKE_OSX_ARCHITECTURES:-$(uname -m)}" \
    -D"CMAKE_OSX_DEPLOYMENT_TARGET=${CMAKE_OSX_DEPLOYMENT_TARGET:-12.0}"
else
  set -- "$@" -G Ninja
fi

for variable in \
  OBS_SRTLA_OBS_PREFIX \
  OBS_SRTLA_OBS_SOURCE_DIR \
  OBS_SRTLA_OBS_IMPORT_LIB_DIR \
  OBS_SRTLA_OBS_RUNTIME_DIR \
  OBS_SRTLA_FFMPEG_ROOT \
  OBS_SRTLA_MBEDTLS_ROOT; do
  eval "value=\${$variable-}"
  if [ -n "$value" ]; then
    case "$variable" in
      OBS_SRTLA_OBS_PREFIX)
        set -- "$@" -D"CMAKE_PREFIX_PATH=$value" ;;
      *)
        set -- "$@" -D"$variable=$value" ;;
    esac
  fi
done

cmake "$@"
cmake --build "$BUILD_DIR" --config "$BUILD_TYPE" --parallel
ctest --test-dir "$BUILD_DIR" -C "$BUILD_TYPE" --output-on-failure
cmake --build "$BUILD_DIR" --config "$BUILD_TYPE" --target package

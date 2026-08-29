cmake_minimum_required(VERSION 3.28)
project(obs-srtla-output VERSION 0.1.1 LANGUAGES C CXX)

if(NOT CMAKE_SIZEOF_VOID_P EQUAL 8)
  message(FATAL_ERROR "obs-srtla-output requires a 64-bit target")
endif()

set(CMAKE_C_STANDARD 11)
set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

include(GNUInstallDirs)
if(APPLE AND NOT CMAKE_OSX_DEPLOYMENT_TARGET)
  set(CMAKE_OSX_DEPLOYMENT_TARGET "12.0" CACHE STRING
      "Minimum macOS deployment target")
endif()
if(WIN32)
  set(_obs_srtla_plugin_dir_default "obs-plugins/64bit")
  set(_obs_srtla_data_dir_default "data/obs-plugins")
elseif(APPLE)
  # macOS plugins are installed as bundles. These cache variables remain
  # available for consumers that include the project from a larger build, but
  # the plugin subdirectory owns the bundle's install layout.
  set(_obs_srtla_plugin_dir_default ".")
  set(_obs_srtla_data_dir_default ".")
elseif(UNIX)
  set(_obs_srtla_plugin_dir_default "${CMAKE_INSTALL_LIBDIR}/obs-plugins")
  set(_obs_srtla_data_dir_default "${CMAKE_INSTALL_DATAROOTDIR}/obs/obs-plugins")
else()
  message(FATAL_ERROR "Unsupported host platform")
endif()

option(OBS_SRTLA_BUILD_TESTS "Build native unit tests" ON)
option(OBS_SRTLA_BUILD_VENDOR_SRT "Build the vendored libsrt fork" ON)
option(OBS_SRTLA_REQUIRE_PLUGIN
       "Fail configuration when the OBS plugin target cannot be built" OFF)
option(OBS_SRTLA_USE_OBS_RUNTIME_DEPS
       "Use Qt/FFmpeg/libobs DLLs supplied by the installed OBS runtime" ON)
set(OBS_SRTLA_OBS_PLUGIN_DIR "${_obs_srtla_plugin_dir_default}" CACHE PATH
    "OBS plugin install directory")
set(OBS_SRTLA_OBS_DATA_DIR "${_obs_srtla_data_dir_default}" CACHE PATH
    "OBS plugin data directory")
set(OBS_SRTLA_USE_OBS_COMPAT_CONFIG OFF)
set(_ffmpeg_root_default "")
if(EXISTS "${CMAKE_CURRENT_SOURCE_DIR}/tools/obs-ffmpeg-dev/include/libavformat/avformat.h")
  set(_ffmpeg_root_default "${CMAKE_CURRENT_SOURCE_DIR}/tools/obs-ffmpeg-dev")
endif()
set(OBS_SRTLA_FFMPEG_ROOT "${_ffmpeg_root_default}" CACHE PATH
    "FFmpeg development prefix used by the OBS plugin")

# OBS builds its Windows SRT dependency with mbedTLS.  Keep the same crypto
# provider for the embedded SRT fork so the plugin does not need OpenSSL DLLs
# and so the provider is identical across the OBS integration and this target.
set(_mbedtls_root_default "")
if(EXISTS "${CMAKE_CURRENT_SOURCE_DIR}/tools/vcpkg/installed/x64-windows/include/mbedtls/aes.h")
  set(_mbedtls_root_default
      "${CMAKE_CURRENT_SOURCE_DIR}/tools/vcpkg/installed/x64-windows")
endif()
set(OBS_SRTLA_MBEDTLS_ROOT "${_mbedtls_root_default}" CACHE PATH
    "mbedTLS development prefix used by the vendored SRT fork")

set(_obs_srtla_mbedtls_include_hints)
set(_obs_srtla_mbedtls_library_hints)
set(_obs_srtla_mbedtls_search_policy)
if(OBS_SRTLA_MBEDTLS_ROOT)
  # The vendored SRT tree uses its legacy FindMbedTLS.cmake module, which
  # expects a prefix containing include/ and lib/.  Make the same prefix
  # explicit for both that module and our direct MSVC test/plugin links.
  set(MBEDTLS_PREFIX "${OBS_SRTLA_MBEDTLS_ROOT}" CACHE PATH
      "The path of mbedTLS" FORCE)
  list(PREPEND CMAKE_PREFIX_PATH "${OBS_SRTLA_MBEDTLS_ROOT}")
  list(APPEND _obs_srtla_mbedtls_include_hints
       "${OBS_SRTLA_MBEDTLS_ROOT}/include")
  list(APPEND _obs_srtla_mbedtls_library_hints
       "${OBS_SRTLA_MBEDTLS_ROOT}/lib")
  set(_obs_srtla_mbedtls_search_policy NO_DEFAULT_PATH)
endif()

# A complete OBS development prefix (as used by GitHub Actions) carries
# mbedTLS alongside libobs, Qt, and FFmpeg.  When no explicit root is supplied,
# allow normal CMake prefix discovery to find that copy.
find_path(OBS_SRTLA_MBEDTLS_INCLUDE_DIR
          NAMES mbedtls/aes.h
          HINTS ${_obs_srtla_mbedtls_include_hints}
          ${_obs_srtla_mbedtls_search_policy})
find_library(OBS_SRTLA_MBEDTLS_LIBRARY
             NAMES mbedtls
             HINTS ${_obs_srtla_mbedtls_library_hints}
             ${_obs_srtla_mbedtls_search_policy})
find_library(OBS_SRTLA_MBEDCRYPTO_LIBRARY
             NAMES mbedcrypto
             HINTS ${_obs_srtla_mbedtls_library_hints}
             ${_obs_srtla_mbedtls_search_policy})
find_library(OBS_SRTLA_MBEDX509_LIBRARY
             NAMES mbedx509
             HINTS ${_obs_srtla_mbedtls_library_hints}
             ${_obs_srtla_mbedtls_search_policy})
if(OBS_SRTLA_MBEDTLS_LIBRARY AND OBS_SRTLA_MBEDCRYPTO_LIBRARY AND
   OBS_SRTLA_MBEDX509_LIBRARY)
  set(OBS_SRTLA_MBEDTLS_LIBRARIES
      "${OBS_SRTLA_MBEDTLS_LIBRARY}"
      "${OBS_SRTLA_MBEDCRYPTO_LIBRARY}"
      "${OBS_SRTLA_MBEDX509_LIBRARY}")
  if(WIN32)
    list(APPEND OBS_SRTLA_MBEDTLS_LIBRARIES bcrypt)
  endif()
endif()

if(OBS_SRTLA_REQUIRE_PLUGIN AND
   (NOT OBS_SRTLA_MBEDTLS_LIBRARY OR NOT OBS_SRTLA_MBEDCRYPTO_LIBRARY OR
    NOT OBS_SRTLA_MBEDX509_LIBRARY))
  message(FATAL_ERROR "mbedTLS development libraries were not found; OBS_SRTLA_REQUIRE_PLUGIN is ON")
endif()

# The binary OBS distribution contains the runtime DLLs, but deliberately does
# not ship headers or import libraries.  When building on Windows we can use
# the matching OBS source tree for headers and generate/import the two small
# import libraries from the installed DLLs.  This keeps libobs dynamically
# linked to the OBS process instead of embedding a second copy of libobs.
set(_obs_source_default "")
if(EXISTS "${CMAKE_CURRENT_SOURCE_DIR}/tools/obs-source/libobs/obs.h")
  set(_obs_source_default "${CMAKE_CURRENT_SOURCE_DIR}/tools/obs-source")
endif()
set(_obs_import_default "")
if(EXISTS "${CMAKE_CURRENT_SOURCE_DIR}/tools/obs-dev/obs.lib")
  set(_obs_import_default "${CMAKE_CURRENT_SOURCE_DIR}/tools/obs-dev")
endif()
set(_obs_runtime_default "")
if(WIN32 AND EXISTS "C:/Program Files/obs-studio/bin/64bit/obs.dll")
  set(_obs_runtime_default "C:/Program Files/obs-studio/bin/64bit")
endif()
set(OBS_SRTLA_OBS_SOURCE_DIR "${_obs_source_default}" CACHE PATH
    "OBS source tree used for plugin headers")
set(OBS_SRTLA_OBS_IMPORT_LIB_DIR "${_obs_import_default}" CACHE PATH
    "Directory containing obs.lib and obs-frontend-api.lib")
set(OBS_SRTLA_OBS_RUNTIME_DIR "${_obs_runtime_default}" CACHE PATH
    "Directory containing the OBS runtime DLLs")

if(OBS_SRTLA_USE_OBS_RUNTIME_DEPS AND DEFINED VCPKG_APPLOCAL_DEPS)
  # Do not let vcpkg copy Qt/FFmpeg into the plugin directory.  OBS already
  # owns those DLLs and the plugin must resolve them from OBS's bin/64bit.
  set(VCPKG_APPLOCAL_DEPS OFF CACHE BOOL "" FORCE)
endif()

find_package(Threads REQUIRED)
find_package(Qt6 QUIET COMPONENTS Widgets)
find_program(OBS_SRTLA_CARGO_EXECUTABLE NAMES cargo cargo.exe REQUIRED)

add_subdirectory(engine)

if(OBS_SRTLA_BUILD_VENDOR_SRT)
  set(ENABLE_SHARED OFF CACHE BOOL "" FORCE)
  set(ENABLE_STATIC ON CACHE BOOL "" FORCE)
  set(ENABLE_APPS OFF CACHE BOOL "" FORCE)
  set(ENABLE_TESTING OFF CACHE BOOL "" FORCE)
  set(ENABLE_UNITTESTS OFF CACHE BOOL "" FORCE)
  set(USE_ENCLIB "mbedtls" CACHE STRING "The crypto library that SRT uses" FORCE)
  set(ENABLE_AEAD_API_PREVIEW OFF CACHE BOOL "" FORCE)
  add_subdirectory(third_party/irlserver-srt EXCLUDE_FROM_ALL)
  if(MSVC AND TARGET srt_virtual)
    # The vendored SRT sources use C++ exceptions in the MSVC build.  OBS's
    # own dependency toolchain enables this flag globally; mirror it here so
    # a clean provider switch does not depend on an inherited developer shell.
    target_compile_options(srt_virtual PRIVATE /EHsc)
  endif()
endif()

set(CPACK_PACKAGE_NAME "obs-srtla-output")
set(CPACK_PACKAGE_VERSION "${PROJECT_VERSION}")
set(CPACK_INCLUDE_TOPLEVEL_DIRECTORY ON)
if(WIN32)
  set(CPACK_GENERATOR "ZIP")
  set(CPACK_SYSTEM_NAME "obs32-windows-x64")
elseif(APPLE)
  set(CPACK_GENERATOR "ZIP")
  set(_obs_srtla_package_arch "${CMAKE_OSX_ARCHITECTURES}")
  if(NOT _obs_srtla_package_arch)
    set(_obs_srtla_package_arch "${CMAKE_SYSTEM_PROCESSOR}")
  endif()
  string(REPLACE ";" "-" _obs_srtla_package_arch "${_obs_srtla_package_arch}")
  string(TOLOWER "${_obs_srtla_package_arch}" _obs_srtla_package_arch)
  if(_obs_srtla_package_arch STREQUAL "amd64")
    set(_obs_srtla_package_arch "x86_64")
  elseif(_obs_srtla_package_arch STREQUAL "aarch64")
    set(_obs_srtla_package_arch "arm64")
  endif()
  set(CPACK_SYSTEM_NAME "macos-${_obs_srtla_package_arch}")
  set(CPACK_PACKAGE_FILE_NAME
      "${CPACK_PACKAGE_NAME}-${CPACK_PACKAGE_VERSION}-${CPACK_SYSTEM_NAME}")
elseif(UNIX)
  set(CPACK_GENERATOR "TGZ")
  set(_obs_srtla_package_arch "${CMAKE_SYSTEM_PROCESSOR}")
  string(TOLOWER "${_obs_srtla_package_arch}" _obs_srtla_package_arch)
  if(_obs_srtla_package_arch STREQUAL "amd64")
    set(_obs_srtla_package_arch "x86_64")
  elseif(_obs_srtla_package_arch STREQUAL "aarch64")
    set(_obs_srtla_package_arch "arm64")
  endif()
  set(CPACK_SYSTEM_NAME "linux-${_obs_srtla_package_arch}")
  set(CPACK_PACKAGE_FILE_NAME
      "${CPACK_PACKAGE_NAME}-${CPACK_PACKAGE_VERSION}-${CPACK_SYSTEM_NAME}")
else()
  message(FATAL_ERROR "Unsupported packaging platform")
endif()
include(CPack)

if(APPLE)
  set(_obs_srtla_metadata_dir "obs-srtla-output.plugin/Contents/Resources/obs-srtla-output")
else()
  set(_obs_srtla_metadata_dir "${OBS_SRTLA_OBS_PLUGIN_DIR}/obs-srtla-output")
endif()

install(FILES "${CMAKE_CURRENT_SOURCE_DIR}/LICENSE"
        DESTINATION "${_obs_srtla_metadata_dir}")
install(FILES
        "${CMAKE_CURRENT_SOURCE_DIR}/third_party/VENDORING.md"
        "${CMAKE_CURRENT_SOURCE_DIR}/third_party/irlserver-srt/LICENSE"
        "${CMAKE_CURRENT_SOURCE_DIR}/third_party/srtla_send/LICENSE"
        DESTINATION "${_obs_srtla_metadata_dir}/licenses")

if(NOT TARGET OBS::libobs)
  find_package(libobs CONFIG QUIET)
endif()

# Standalone OBS SDKs export the frontend package separately and name its
# imported target OBS::obs-frontend-api.  Normalize it to the target name used
# by in-tree OBS plugin builds and by our runtime-import fallback below.
if(TARGET OBS::libobs AND NOT TARGET OBS::frontend-api)
  find_package(obs-frontend-api CONFIG QUIET)
  if(TARGET OBS::obs-frontend-api)
    add_library(OBS::frontend-api ALIAS OBS::obs-frontend-api)
  endif()
endif()

# Fallback for the normal OBS installer.  It has obs.dll and
# obs-frontend-api.dll but no CMake package.  The generated import libraries
# are ordinary MSVC import libs and do not copy or statically link OBS code.
if(NOT TARGET OBS::libobs AND WIN32 AND
   EXISTS "${OBS_SRTLA_OBS_SOURCE_DIR}/libobs/obs.h" AND
   EXISTS "${OBS_SRTLA_OBS_IMPORT_LIB_DIR}/obs.lib" AND
   EXISTS "${OBS_SRTLA_OBS_IMPORT_LIB_DIR}/obs-frontend-api.lib")
  set(_obs_include_dirs
      "${OBS_SRTLA_OBS_SOURCE_DIR}/libobs"
      "${OBS_SRTLA_OBS_SOURCE_DIR}/libobs/media-io"
      "${OBS_SRTLA_OBS_SOURCE_DIR}/libobs/util"
      "${OBS_SRTLA_OBS_SOURCE_DIR}/libobs/graphics"
      "${OBS_SRTLA_OBS_SOURCE_DIR}/libobs/callback")

  add_library(OBS::libobs SHARED IMPORTED GLOBAL)
  set_target_properties(OBS::libobs PROPERTIES
    IMPORTED_IMPLIB "${OBS_SRTLA_OBS_IMPORT_LIB_DIR}/obs.lib"
    INTERFACE_INCLUDE_DIRECTORIES "${_obs_include_dirs}")
  if(EXISTS "${OBS_SRTLA_OBS_RUNTIME_DIR}/obs.dll")
    set_property(TARGET OBS::libobs PROPERTY IMPORTED_LOCATION
                 "${OBS_SRTLA_OBS_RUNTIME_DIR}/obs.dll")
  endif()

  add_library(OBS::frontend-api SHARED IMPORTED GLOBAL)
  set_target_properties(OBS::frontend-api PROPERTIES
    IMPORTED_IMPLIB "${OBS_SRTLA_OBS_IMPORT_LIB_DIR}/obs-frontend-api.lib"
    INTERFACE_INCLUDE_DIRECTORIES
      "${OBS_SRTLA_OBS_SOURCE_DIR}/frontend/api;${_obs_include_dirs}"
    INTERFACE_LINK_LIBRARIES OBS::libobs)
  if(EXISTS "${OBS_SRTLA_OBS_RUNTIME_DIR}/obs-frontend-api.dll")
    set_property(TARGET OBS::frontend-api PROPERTY IMPORTED_LOCATION
                 "${OBS_SRTLA_OBS_RUNTIME_DIR}/obs-frontend-api.dll")
  endif()
  set(OBS_SRTLA_USE_OBS_COMPAT_CONFIG ON)
  message(STATUS "Using OBS runtime DLLs with generated import libraries from ${OBS_SRTLA_OBS_IMPORT_LIB_DIR}")
endif()

if(NOT TARGET OBS::libobs)
  if(OBS_SRTLA_REQUIRE_PLUGIN)
    message(FATAL_ERROR "OBS development targets were not found; OBS_SRTLA_REQUIRE_PLUGIN is ON")
  endif()
  message(WARNING "OBS development targets were not found; only the Rust/ABR slice can be built.")
elseif(NOT TARGET OBS::frontend-api)
  if(OBS_SRTLA_REQUIRE_PLUGIN)
    message(FATAL_ERROR "OBS frontend API target was not found; OBS_SRTLA_REQUIRE_PLUGIN is ON")
  endif()
  message(WARNING "OBS frontend API target was not found; OBS plugin target is disabled.")
elseif(NOT TARGET Qt6::Widgets)
  if(OBS_SRTLA_REQUIRE_PLUGIN)
    message(FATAL_ERROR "Qt6 Widgets was not found; OBS_SRTLA_REQUIRE_PLUGIN is ON")
  endif()
  message(WARNING "Qt6 Widgets was not found; OBS plugin target is disabled.")
else()
  set(_obs_srtla_ffmpeg_include_hints)
  set(_obs_srtla_ffmpeg_library_hints)
  if(OBS_SRTLA_FFMPEG_ROOT)
    list(APPEND _obs_srtla_ffmpeg_include_hints
         "${OBS_SRTLA_FFMPEG_ROOT}/include")
    list(APPEND _obs_srtla_ffmpeg_library_hints
         "${OBS_SRTLA_FFMPEG_ROOT}/lib")
  endif()
  if(DEFINED ENV{OBS_SRTLA_FFMPEG_ROOT} AND
     NOT "$ENV{OBS_SRTLA_FFMPEG_ROOT}" STREQUAL "")
    list(APPEND _obs_srtla_ffmpeg_include_hints
         "$ENV{OBS_SRTLA_FFMPEG_ROOT}/include")
    list(APPEND _obs_srtla_ffmpeg_library_hints
         "$ENV{OBS_SRTLA_FFMPEG_ROOT}/lib")
  endif()
  find_path(OBS_SRTLA_FFMPEG_INCLUDE_DIR
            NAMES libavformat/avformat.h
            HINTS ${_obs_srtla_ffmpeg_include_hints})
  find_library(OBS_SRTLA_FFMPEG_AVFORMAT
               NAMES avformat avformat-61 avformat-60 avformat-59
               HINTS ${_obs_srtla_ffmpeg_library_hints})
  find_library(OBS_SRTLA_FFMPEG_AVCODEC
               NAMES avcodec avcodec-61 avcodec-60 avcodec-59
               HINTS ${_obs_srtla_ffmpeg_library_hints})
  find_library(OBS_SRTLA_FFMPEG_AVUTIL
               NAMES avutil avutil-59 avutil-58 avutil-57
               HINTS ${_obs_srtla_ffmpeg_library_hints})
  if(NOT OBS_SRTLA_FFMPEG_INCLUDE_DIR OR NOT OBS_SRTLA_FFMPEG_AVFORMAT OR
     NOT OBS_SRTLA_FFMPEG_AVCODEC OR NOT OBS_SRTLA_FFMPEG_AVUTIL)
    if(OBS_SRTLA_REQUIRE_PLUGIN)
      message(FATAL_ERROR "FFmpeg development headers/libraries were not found; OBS_SRTLA_REQUIRE_PLUGIN is ON")
    endif()
    message(WARNING "FFmpeg development headers/libraries were not found; the OBS plugin target is disabled. Set OBS_SRTLA_FFMPEG_ROOT to an FFmpeg development prefix.")
  else()
    set(OBS_SRTLA_FFMPEG_LIBRARIES
        "${OBS_SRTLA_FFMPEG_AVFORMAT}"
        "${OBS_SRTLA_FFMPEG_AVCODEC}"
        "${OBS_SRTLA_FFMPEG_AVUTIL}")
    add_subdirectory(plugin)
  endif()
endif()

if(OBS_SRTLA_REQUIRE_PLUGIN AND NOT TARGET obs-srtla-output)
  message(FATAL_ERROR "obs-srtla-output plugin target was not created; OBS_SRTLA_REQUIRE_PLUGIN is ON")
endif()

if(OBS_SRTLA_BUILD_TESTS)
  enable_testing()
  add_test(NAME srtla_engine_rust_tests
           COMMAND "${OBS_SRTLA_CARGO_EXECUTABLE}" test --manifest-path "${CMAKE_CURRENT_SOURCE_DIR}/engine/Cargo.toml")
  if(TARGET srt_static)
    add_executable(srt_transport_abi_test tests/srt_transport_abi_test.cpp)
    target_include_directories(srt_transport_abi_test PRIVATE
      third_party/irlserver-srt/srtcore
      third_party/irlserver-srt/common
      "${OBS_SRTLA_MBEDTLS_INCLUDE_DIR}"
      "${CMAKE_CURRENT_BINARY_DIR}/third_party/irlserver-srt")
    target_link_libraries(srt_transport_abi_test PRIVATE srt_static Threads::Threads)
    if(OBS_SRTLA_MBEDTLS_LIBRARIES)
      target_link_libraries(srt_transport_abi_test PRIVATE ${OBS_SRTLA_MBEDTLS_LIBRARIES})
    endif()
    add_test(NAME srt_transport_abi_test COMMAND srt_transport_abi_test)
  endif()
  add_executable(output_capture_lifecycle_test tests/output_capture_lifecycle_test.cpp)
  target_include_directories(output_capture_lifecycle_test PRIVATE plugin)
  add_test(NAME output_capture_lifecycle_test COMMAND output_capture_lifecycle_test)
  if(TARGET Qt6::Core)
    add_executable(srtla_stats_json_test tests/stats_json_test.cpp)
    target_include_directories(srtla_stats_json_test PRIVATE plugin)
    target_link_libraries(srtla_stats_json_test PRIVATE Qt6::Core)
    add_test(NAME srtla_stats_json_test COMMAND srtla_stats_json_test)
    if(WIN32)
      set_tests_properties(srtla_stats_json_test PROPERTIES
        ENVIRONMENT_MODIFICATION
          "PATH=path_list_prepend:$<TARGET_FILE_DIR:Qt6::Core>")
    endif()
  endif()
  add_executable(srtla_passphrase_store_test
    tests/passphrase_store_test.cpp plugin/passphrase-store.cpp)
  target_include_directories(srtla_passphrase_store_test PRIVATE plugin)
  add_test(NAME srtla_passphrase_store_test COMMAND srtla_passphrase_store_test)
endif()

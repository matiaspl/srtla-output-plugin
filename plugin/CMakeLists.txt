if(WIN32)
  set(OBS_SRTLA_NETWORK_MONITOR_SOURCE network-monitor-windows.cpp)
else()
  set(OBS_SRTLA_NETWORK_MONITOR_SOURCE network-monitor-posix.cpp)
endif()

add_library(obs-srtla-output MODULE
  obs-srtla-module.cpp
  obs-srtla-output.cpp
  obs-srtla-dock.cpp
  obs-srtla-dock.hpp
  stats-json.hpp
  output-capture-lifecycle.hpp
  mpegts-avformat-sink.cpp
  mpegts-avformat-sink.hpp
  passphrase-store.cpp
  passphrase-store.hpp
  srt-session.cpp
  srt-session.hpp
  ${OBS_SRTLA_NETWORK_MONITOR_SOURCE}
  network-monitor.hpp
)

target_include_directories(obs-srtla-output PRIVATE
  ${CMAKE_CURRENT_SOURCE_DIR}
  ${CMAKE_CURRENT_SOURCE_DIR}/../engine/include
  ${CMAKE_CURRENT_SOURCE_DIR}/../third_party/irlserver-srt/srtcore
  ${CMAKE_CURRENT_SOURCE_DIR}/../third_party/irlserver-srt/common
  ${CMAKE_BINARY_DIR}/third_party/irlserver-srt
  ${OBS_SRTLA_MBEDTLS_INCLUDE_DIR}
  ${OBS_SRTLA_FFMPEG_INCLUDE_DIR}
)

if(OBS_SRTLA_USE_OBS_COMPAT_CONFIG)
  target_include_directories(obs-srtla-output PRIVATE
    ${CMAKE_CURRENT_SOURCE_DIR}/obs-compat)
endif()

target_link_libraries(obs-srtla-output PRIVATE
  OBS::libobs
  OBS::frontend-api
  obs-srtla-engine-lib
  Qt6::Widgets
  Threads::Threads
  ${OBS_SRTLA_MBEDTLS_LIBRARIES}
  ${OBS_SRTLA_FFMPEG_LIBRARIES}
)

if(TARGET srt_static)
  target_link_libraries(obs-srtla-output PRIVATE srt_static)
endif()

add_dependencies(obs-srtla-output obs-srtla-engine)

if(WIN32)
  target_compile_definitions(obs-srtla-output PRIVATE
    UNICODE _UNICODE WIN32_LEAN_AND_MEAN NOMINMAX)
endif()

if(MSVC)
  # Match OBS's MSVC exception model.  The Qt/FFmpeg/SRT glue uses C++
  # exceptions internally and must not be compiled with /EHs-.
  target_compile_options(obs-srtla-output PRIVATE /EHsc)
endif()

set_target_properties(obs-srtla-output PROPERTIES
  OUTPUT_NAME obs-srtla-output
  PREFIX ""
)

if(APPLE)
  file(GLOB_RECURSE OBS_SRTLA_BUNDLE_RESOURCES CONFIGURE_DEPENDS
    "${CMAKE_CURRENT_SOURCE_DIR}/data/*")
  foreach(resource IN LISTS OBS_SRTLA_BUNDLE_RESOURCES)
    if(IS_DIRECTORY "${resource}")
      continue()
    endif()
    file(RELATIVE_PATH relative_resource
      "${CMAKE_CURRENT_SOURCE_DIR}/data" "${resource}")
    get_filename_component(relative_resource_dir "${relative_resource}" DIRECTORY)
    target_sources(obs-srtla-output PRIVATE "${resource}")
    set_source_files_properties("${resource}" PROPERTIES
      MACOSX_PACKAGE_LOCATION "Resources/${relative_resource_dir}")
  endforeach()
  set_target_properties(obs-srtla-output PROPERTIES
    BUNDLE TRUE
    BUNDLE_EXTENSION plugin
    XCODE_ATTRIBUTE_PRODUCT_NAME obs-srtla-output
    XCODE_ATTRIBUTE_PRODUCT_BUNDLE_IDENTIFIER com.obsproject.obs-srtla-output
    XCODE_ATTRIBUTE_GENERATE_INFOPLIST_FILE YES
    XCODE_ATTRIBUTE_INFOPLIST_FILE ""
    XCODE_ATTRIBUTE_INFOPLIST_KEY_CFBundleDisplayName obs-srtla-output
    XCODE_ATTRIBUTE_CODE_SIGN_IDENTITY "-"
    XCODE_ATTRIBUTE_CODE_SIGNING_ALLOWED NO
    XCODE_ATTRIBUTE_CODE_SIGNING_REQUIRED NO
    XCODE_ATTRIBUTE_INSTALL_PATH
      "$(USER_LIBRARY_DIR)/Application Support/obs-studio/plugins")
  install(TARGETS obs-srtla-output
    LIBRARY DESTINATION ".")
  install(FILES "${CMAKE_CURRENT_SOURCE_DIR}/../README.md"
    DESTINATION "obs-srtla-output.plugin/Contents/Resources/obs-srtla-output")
else()
  install(TARGETS obs-srtla-output
    RUNTIME DESTINATION "${OBS_SRTLA_OBS_PLUGIN_DIR}"
    LIBRARY DESTINATION "${OBS_SRTLA_OBS_PLUGIN_DIR}")
  install(FILES "${CMAKE_CURRENT_SOURCE_DIR}/../README.md"
    DESTINATION "${OBS_SRTLA_OBS_PLUGIN_DIR}/obs-srtla-output")
  install(DIRECTORY "${CMAKE_CURRENT_SOURCE_DIR}/data/"
    DESTINATION "${OBS_SRTLA_OBS_DATA_DIR}/obs-srtla-output")
endif()

# mbedTLS is statically linked, matching the OBS SRT dependency.  Preserve its
# license in the plugin package without shipping an additional crypto DLL.
if(OBS_SRTLA_MBEDTLS_ROOT AND
   EXISTS "${OBS_SRTLA_MBEDTLS_ROOT}/share/mbedtls/copyright")
  if(APPLE)
    set(_obs_srtla_mbedtls_license_dir
        "obs-srtla-output.plugin/Contents/Resources/obs-srtla-output/licenses")
  else()
    set(_obs_srtla_mbedtls_license_dir
        "${OBS_SRTLA_OBS_PLUGIN_DIR}/obs-srtla-output/licenses")
  endif()
  install(FILES "${OBS_SRTLA_MBEDTLS_ROOT}/share/mbedtls/copyright"
          DESTINATION "${_obs_srtla_mbedtls_license_dir}"
          RENAME mbedtls-copyright)
endif()

if(WIN32)
  target_link_libraries(obs-srtla-output PRIVATE iphlpapi ws2_32 ntdll)
endif()

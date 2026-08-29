if(WIN32)
  set(OBS_SRTLA_NETWORK_MONITOR_SOURCE network-monitor-windows.cpp)
else()
  set(OBS_SRTLA_NETWORK_MONITOR_SOURCE network-monitor-posix.cpp)
endif()

add_library(${SRTLA_PLUGIN_NAME} MODULE
  srtla-module.cpp
  srtla-output.cpp
  srtla-dock.cpp
  srtla-dock.hpp
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

target_include_directories(${SRTLA_PLUGIN_NAME} PRIVATE
  ${CMAKE_CURRENT_SOURCE_DIR}
  ${CMAKE_CURRENT_SOURCE_DIR}/../engine/include
  ${CMAKE_CURRENT_SOURCE_DIR}/../third_party/irlserver-srt/srtcore
  ${CMAKE_CURRENT_SOURCE_DIR}/../third_party/irlserver-srt/common
  ${CMAKE_BINARY_DIR}/third_party/irlserver-srt
  ${OBS_SRTLA_MBEDTLS_INCLUDE_DIR}
  ${OBS_SRTLA_FFMPEG_INCLUDE_DIR}
)

if(OBS_SRTLA_USE_OBS_COMPAT_CONFIG)
  target_include_directories(${SRTLA_PLUGIN_NAME} PRIVATE
    ${CMAKE_CURRENT_SOURCE_DIR}/obs-compat)
endif()

target_link_libraries(${SRTLA_PLUGIN_NAME} PRIVATE
  OBS::libobs
  OBS::frontend-api
  srtla-engine-lib
  Qt6::Widgets
  Threads::Threads
  ${OBS_SRTLA_MBEDTLS_LIBRARIES}
  ${OBS_SRTLA_FFMPEG_LIBRARIES}
)

if(TARGET srt_static)
  target_link_libraries(${SRTLA_PLUGIN_NAME} PRIVATE srt_static)
endif()

add_dependencies(${SRTLA_PLUGIN_NAME} srtla-engine)

if(WIN32)
  target_compile_definitions(${SRTLA_PLUGIN_NAME} PRIVATE
    UNICODE _UNICODE WIN32_LEAN_AND_MEAN NOMINMAX)
endif()

if(MSVC)
  # Match OBS's MSVC exception model.  The Qt/FFmpeg/SRT glue uses C++
  # exceptions internally and must not be compiled with /EHs-.
  target_compile_options(${SRTLA_PLUGIN_NAME} PRIVATE /EHsc)
endif()

set_target_properties(${SRTLA_PLUGIN_NAME} PROPERTIES
  OUTPUT_NAME ${SRTLA_PLUGIN_NAME}
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
    target_sources(${SRTLA_PLUGIN_NAME} PRIVATE "${resource}")
    set_source_files_properties("${resource}" PROPERTIES
      MACOSX_PACKAGE_LOCATION "Resources/${relative_resource_dir}")
  endforeach()
  set_target_properties(${SRTLA_PLUGIN_NAME} PROPERTIES
    BUNDLE TRUE
    BUNDLE_EXTENSION plugin
    XCODE_ATTRIBUTE_PRODUCT_NAME ${SRTLA_PLUGIN_NAME}
    XCODE_ATTRIBUTE_PRODUCT_BUNDLE_IDENTIFIER ${SRTLA_PLUGIN_BUNDLE_ID}
    XCODE_ATTRIBUTE_GENERATE_INFOPLIST_FILE YES
    XCODE_ATTRIBUTE_INFOPLIST_FILE ""
    XCODE_ATTRIBUTE_INFOPLIST_KEY_CFBundleDisplayName ${SRTLA_PLUGIN_NAME}
    XCODE_ATTRIBUTE_CODE_SIGN_IDENTITY "-"
    XCODE_ATTRIBUTE_CODE_SIGNING_ALLOWED NO
    XCODE_ATTRIBUTE_CODE_SIGNING_REQUIRED NO
    XCODE_ATTRIBUTE_INSTALL_PATH
      "$(USER_LIBRARY_DIR)/Application Support/obs-studio/plugins")
  install(TARGETS ${SRTLA_PLUGIN_NAME}
    LIBRARY DESTINATION ".")
  install(FILES "${CMAKE_CURRENT_SOURCE_DIR}/../README.md"
    DESTINATION "${SRTLA_PLUGIN_NAME}.plugin/Contents/Resources/${SRTLA_PLUGIN_NAME}")
else()
  install(TARGETS ${SRTLA_PLUGIN_NAME}
    RUNTIME DESTINATION "${OBS_SRTLA_OBS_PLUGIN_DIR}"
    LIBRARY DESTINATION "${OBS_SRTLA_OBS_PLUGIN_DIR}")
  install(FILES "${CMAKE_CURRENT_SOURCE_DIR}/../README.md"
    DESTINATION "${OBS_SRTLA_OBS_PLUGIN_DIR}/${SRTLA_PLUGIN_NAME}")
  install(DIRECTORY "${CMAKE_CURRENT_SOURCE_DIR}/data/"
    DESTINATION "${OBS_SRTLA_OBS_DATA_DIR}/${SRTLA_PLUGIN_NAME}")
endif()

# mbedTLS is statically linked, matching the OBS SRT dependency.  Preserve its
# license in the plugin package without shipping an additional crypto DLL.
if(OBS_SRTLA_MBEDTLS_ROOT AND
   EXISTS "${OBS_SRTLA_MBEDTLS_ROOT}/share/mbedtls/copyright")
  if(APPLE)
    set(_obs_srtla_mbedtls_license_dir
        "${SRTLA_PLUGIN_NAME}.plugin/Contents/Resources/${SRTLA_PLUGIN_NAME}/licenses")
  else()
    set(_obs_srtla_mbedtls_license_dir
        "${OBS_SRTLA_OBS_PLUGIN_DIR}/${SRTLA_PLUGIN_NAME}/licenses")
  endif()
  install(FILES "${OBS_SRTLA_MBEDTLS_ROOT}/share/mbedtls/copyright"
          DESTINATION "${_obs_srtla_mbedtls_license_dir}"
          RENAME mbedtls-copyright)
endif()

if(WIN32)
  target_link_libraries(${SRTLA_PLUGIN_NAME} PRIVATE iphlpapi ws2_32 ntdll)
endif()

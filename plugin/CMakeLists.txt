add_library(obs-srtla-output MODULE
  obs-srtla-module.cpp
  obs-srtla-output.cpp
  obs-srtla-dock.cpp
  obs-srtla-dock.hpp
  output-capture-lifecycle.hpp
  mpegts-avformat-sink.cpp
  mpegts-avformat-sink.hpp
  secret-store.cpp
  secret-store.hpp
  srt-session.cpp
  srt-session.hpp
  network-monitor-windows.cpp
  network-monitor.hpp
)

target_include_directories(obs-srtla-output PRIVATE
  ${CMAKE_CURRENT_SOURCE_DIR}
  ${CMAKE_CURRENT_SOURCE_DIR}/obs-compat
  ${CMAKE_CURRENT_SOURCE_DIR}/../engine/include
  ${CMAKE_CURRENT_SOURCE_DIR}/../third_party/irlserver-srt/srtcore
  ${CMAKE_CURRENT_SOURCE_DIR}/../third_party/irlserver-srt/common
  ${CMAKE_BINARY_DIR}/third_party/irlserver-srt
  ${OBS_SRTLA_FFMPEG_INCLUDE_DIR}
)

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

target_compile_definitions(obs-srtla-output PRIVATE
  UNICODE _UNICODE WIN32_LEAN_AND_MEAN NOMINMAX
)

if(MSVC)
  # Match OBS's MSVC exception model.  The Qt/FFmpeg/SRT glue uses C++
  # exceptions internally and must not be compiled with /EHs-.
  target_compile_options(obs-srtla-output PRIVATE /EHsc)
endif()

set_target_properties(obs-srtla-output PROPERTIES
  OUTPUT_NAME obs-srtla-output
  PREFIX ""
)

install(TARGETS obs-srtla-output
  RUNTIME DESTINATION "${OBS_SRTLA_OBS_PLUGIN_DIR}"
  LIBRARY DESTINATION "${OBS_SRTLA_OBS_PLUGIN_DIR}"
)
install(FILES "${CMAKE_CURRENT_SOURCE_DIR}/../README.md"
  DESTINATION "${OBS_SRTLA_OBS_PLUGIN_DIR}/obs-srtla-output"
)
install(DIRECTORY "${CMAKE_CURRENT_SOURCE_DIR}/data/"
  DESTINATION "${OBS_SRTLA_OBS_DATA_DIR}/obs-srtla-output"
)

# mbedTLS is statically linked, matching the OBS SRT dependency.  Preserve its
# license in the plugin package without shipping an additional crypto DLL.
if(OBS_SRTLA_MBEDTLS_ROOT AND
   EXISTS "${OBS_SRTLA_MBEDTLS_ROOT}/share/mbedtls/copyright")
  install(FILES "${OBS_SRTLA_MBEDTLS_ROOT}/share/mbedtls/copyright"
          DESTINATION "${OBS_SRTLA_OBS_PLUGIN_DIR}/obs-srtla-output/licenses"
          RENAME mbedtls-copyright)
endif()

if(WIN32)
  target_link_libraries(obs-srtla-output PRIVATE iphlpapi ws2_32 crypt32 ntdll)
endif()

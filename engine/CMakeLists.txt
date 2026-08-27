if(WIN32)
  set(OBS_SRTLA_RUST_OUTPUT "${CMAKE_CURRENT_BINARY_DIR}/libobs_srtla_engine.lib")
  set(OBS_SRTLA_RUST_SOURCE "${CMAKE_CURRENT_SOURCE_DIR}/target/release/obs_srtla_engine.lib")
else()
  set(OBS_SRTLA_RUST_OUTPUT "${CMAKE_CURRENT_BINARY_DIR}/libobs_srtla_engine.a")
  set(OBS_SRTLA_RUST_SOURCE "${CMAKE_CURRENT_SOURCE_DIR}/target/release/libobs_srtla_engine.a")
endif()

# Cargo tracks path dependencies correctly, but CMake still needs to know when
# their sources invalidate the copied static library. Keep this list recursive
# so edits in srtla-core (for example selection/link_cc.rs) cannot leave an
# apparently up-to-date plugin linked against stale Rust code.
file(GLOB_RECURSE OBS_SRTLA_RUST_SOURCES CONFIGURE_DEPENDS
  "${CMAKE_CURRENT_SOURCE_DIR}/src/*.rs"
  "${CMAKE_CURRENT_SOURCE_DIR}/../third_party/srtla_send/src/*.rs"
  "${CMAKE_CURRENT_SOURCE_DIR}/../third_party/srtla_send/crates/*/src/*.rs"
)
file(GLOB OBS_SRTLA_RUST_MANIFESTS CONFIGURE_DEPENDS
  "${CMAKE_CURRENT_SOURCE_DIR}/../third_party/srtla_send/crates/*/Cargo.toml"
)

add_custom_command(
  OUTPUT ${OBS_SRTLA_RUST_OUTPUT}
  COMMAND "${OBS_SRTLA_CARGO_EXECUTABLE}" build --manifest-path "${CMAKE_CURRENT_SOURCE_DIR}/Cargo.toml" --release
  COMMAND ${CMAKE_COMMAND} -E copy_if_different
          "${OBS_SRTLA_RUST_SOURCE}"
          "${OBS_SRTLA_RUST_OUTPUT}"
  WORKING_DIRECTORY "${CMAKE_CURRENT_SOURCE_DIR}"
  DEPENDS
    Cargo.toml
    include/srtla_engine.h
    ../third_party/srtla_send/Cargo.toml
    ../third_party/srtla_send/Cargo.lock
    ../third_party/srtla_send/build.rs
    ${OBS_SRTLA_RUST_SOURCES}
    ${OBS_SRTLA_RUST_MANIFESTS}
)

add_custom_target(obs-srtla-engine ALL
  DEPENDS ${OBS_SRTLA_RUST_OUTPUT}
)

add_library(obs-srtla-engine-lib STATIC IMPORTED GLOBAL)
set_target_properties(obs-srtla-engine-lib PROPERTIES
  IMPORTED_LOCATION "${OBS_SRTLA_RUST_OUTPUT}"
  INTERFACE_INCLUDE_DIRECTORIES "${CMAKE_CURRENT_SOURCE_DIR}/include"
)
add_dependencies(obs-srtla-engine-lib obs-srtla-engine)

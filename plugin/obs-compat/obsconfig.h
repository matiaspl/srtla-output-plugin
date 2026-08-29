#pragma once

// The OBS binary installer does not ship the generated obsconfig.h that is
// normally produced while building libobs. This fallback is used only by the
// Windows runtime-import path; native OBS SDKs provide their own generated
// header and must not be shadowed by this file.
#define OBS_DATA_PATH "data"
#define OBS_PLUGIN_PATH "obs-plugins/64bit"
#define OBS_PLUGIN_DESTINATION "obs-plugins/64bit"
#define OBS_RELEASE_CANDIDATE 0
#define OBS_BETA 0
#define OBS_INSTALL_PREFIX ""

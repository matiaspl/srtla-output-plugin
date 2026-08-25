#pragma once

// The OBS binary installer does not ship the generated obsconfig.h that is
// normally produced while building libobs.  These values describe the normal
// Windows plugin layout and are only used by public OBS headers; runtime data
// and plugin loading are still handled by the installed OBS process.
#define OBS_DATA_PATH "data"
#define OBS_PLUGIN_PATH "obs-plugins/64bit"
#define OBS_PLUGIN_DESTINATION "obs-plugins/64bit"
#define OBS_RELEASE_CANDIDATE 0
#define OBS_BETA 0
#define OBS_INSTALL_PREFIX ""

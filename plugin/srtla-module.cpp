#include <obs-module.h>
#include <obs-frontend-api.h>

#include "srtla-dock.hpp"

OBS_DECLARE_MODULE()
OBS_MODULE_USE_DEFAULT_LOCALE("srtla-output", "en-US")

extern struct obs_output_info srtla_output_info;

static SrtlaDock *dock = nullptr;

static void frontend_event(enum obs_frontend_event event, void *)
{
	if (!dock)
		return;
	if (event == OBS_FRONTEND_EVENT_PROFILE_CHANGING) {
		dock->stopOutput();
		return;
	}
	if (event == OBS_FRONTEND_EVENT_PROFILE_CHANGED) {
		dock->reloadProfile();
		return;
	}
	obs_output_t *other = nullptr;
	if (event == OBS_FRONTEND_EVENT_STREAMING_STARTING)
		other = obs_frontend_get_streaming_output();
	else if (event == OBS_FRONTEND_EVENT_RECORDING_STARTING)
		other = obs_frontend_get_recording_output();
	else if (event == OBS_FRONTEND_EVENT_REPLAY_BUFFER_STARTING)
		other = obs_frontend_get_replay_buffer_output();
	if (other) {
		const bool conflict = dock->sharesEncoderWith(other);
		obs_output_release(other);
		if (conflict)
			dock->stopOutput();
	}
}

MODULE_EXPORT const char *obs_module_description(void)
{
	return "Native SRTLA output for OBS";
}

bool obs_module_load(void)
{
	obs_register_output(&srtla_output_info);
	dock = new SrtlaDock();
	obs_frontend_add_event_callback(frontend_event, nullptr);
	if (!obs_frontend_add_dock_by_id("srtla-output-dock", "SRTLA Output", dock)) {
		obs_frontend_remove_event_callback(frontend_event, nullptr);
		delete dock;
		dock = nullptr;
		return false;
	}
	return true;
}

void obs_module_unload(void)
{
	obs_frontend_remove_event_callback(frontend_event, nullptr);
	// obs_frontend_add_dock_by_id transfers ownership of the QWidget to OBS.
	// obs_frontend_remove_dock() drops OBS's shared_ptr and destroys the dock;
	// deleting it again here leaves the dock's output pointer dangling and can
	// crash in obs_output_release during OBS shutdown.
	if (dock)
		obs_frontend_remove_dock("srtla-output-dock");
	dock = nullptr;
}

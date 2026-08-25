#include <obs-module.h>
#include <obs-frontend-api.h>

#include "obs-srtla-dock.hpp"

OBS_DECLARE_MODULE()
OBS_MODULE_USE_DEFAULT_LOCALE("obs-srtla-output", "en-US")

extern struct obs_output_info srtla_output_info;

static SrtlaDock *dock = nullptr;

static void frontend_event(enum obs_frontend_event event, void *)
{
	if (event == OBS_FRONTEND_EVENT_STREAMING_STARTING && dock)
		// Shared encoders cannot be used by two active outputs.  Stop the
		// dock-owned SRTLA output before OBS starts its primary stream.
		dock->stopOutput();
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
	if (!obs_frontend_add_dock_by_id("obs-srtla-output-dock", "SRTLA Output", dock)) {
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
	obs_frontend_remove_dock("obs-srtla-output-dock");
	delete dock;
	dock = nullptr;
}

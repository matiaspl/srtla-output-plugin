#include "srtla-websocket.hpp"

#include "srtla-dock.hpp"

#include <obs.h>

#include <QByteArray>
#include <QMetaObject>
#include <QThread>

#include <atomic>
#include <charconv>
#include <cstdint>
#include <limits>
#include <utility>

namespace {

constexpr char vendor_name[] = "obs-srtla-output";

using RequestCallback = void (*)(obs_data_t *, obs_data_t *, void *);

// This is the callback payload used by the public obs-websocket vendor API.
struct WebsocketRequestCallback {
	RequestCallback callback;
	void *priv_data;
};

proc_handler_t *websocket_proc_handler = nullptr;
void *websocket_vendor = nullptr;
std::atomic<SrtlaDock *> dock{nullptr};

bool call_websocket_proc(const char *name, calldata_t *data)
{
	return websocket_proc_handler && proc_handler_call(websocket_proc_handler, name, data);
}

bool register_request(const char *type, RequestCallback callback)
{
	WebsocketRequestCallback request{callback, nullptr};
	calldata_t data{};
	calldata_set_ptr(&data, "vendor", websocket_vendor);
	calldata_set_string(&data, "type", type);
	calldata_set_ptr(&data, "callback", &request);
	call_websocket_proc("vendor_request_register", &data);
	const bool success = calldata_bool(&data, "success");
	calldata_free(&data);
	return success;
}

void unregister_request(const char *type)
{
	calldata_t data{};
	calldata_set_ptr(&data, "vendor", websocket_vendor);
	calldata_set_string(&data, "type", type);
	call_websocket_proc("vendor_request_unregister", &data);
	calldata_free(&data);
}

void set_error(obs_data_t *response, const char *message)
{
	obs_data_set_bool(response, "success", false);
	obs_data_set_string(response, "error", message);
}

void set_error(obs_data_t *response, const QString &message)
{
	const auto utf8 = message.toUtf8();
	set_error(response, utf8.constData());
}

template<typename Function>
bool on_dock_thread(Function &&function)
{
	auto *target = dock.load();
	if (!target)
		return false;
	if (QThread::currentThread() == target->thread()) {
		function(target);
		return true;
	}
	return QMetaObject::invokeMethod(target,
		[target, callback = std::forward<Function>(function)]() mutable { callback(target); },
		Qt::BlockingQueuedConnection);
}

void request_get_status(obs_data_t *, obs_data_t *response, void *)
{
	bool copied = false;
	const bool invoked = on_dock_thread([&](SrtlaDock *target) {
		const auto json = target->websocketStatusJson();
		if (json.isEmpty())
			return;
		auto *status = obs_data_create_from_json(json.constData());
		if (!status)
			return;
		obs_data_apply(response, status);
		obs_data_release(status);
		copied = true;
	});
	if (!invoked || !copied)
		set_error(response, "SRTLA status is unavailable");
}

void request_set_output_active(obs_data_t *request, obs_data_t *response, void *)
{
	if (!obs_data_has_user_value(request, "active")) {
		set_error(response, "active is required");
		return;
	}
	const auto active = obs_data_get_bool(request, "active");
	QString error;
	bool changed = false;
	const bool invoked = on_dock_thread([&](SrtlaDock *target) {
		changed = target->websocketSetOutputActive(active, error);
	});
	if (!invoked || !changed) {
		set_error(response, invoked ? error : QStringLiteral("SRTLA dock is unavailable"));
		return;
	}
	obs_data_set_bool(response, "success", true);
	obs_data_set_bool(response, "active", active);
}

void request_set_link_enabled(obs_data_t *request, obs_data_t *response, void *)
{
	if (!obs_data_has_user_value(request, "linkId") || !obs_data_has_user_value(request, "enabled")) {
		set_error(response, "linkId and enabled are required");
		return;
	}
	const char *linkIdText = obs_data_get_string(request, "linkId");
	if (!linkIdText || !*linkIdText) {
		set_error(response, "linkId must be a decimal string");
		return;
	}
	std::uint64_t linkId = 0;
	const auto *end = linkIdText + std::char_traits<char>::length(linkIdText);
	const auto parsed = std::from_chars(linkIdText, end, linkId);
	if (parsed.ec != std::errc{} || parsed.ptr != end) {
		set_error(response, "linkId must be a non-negative decimal string");
		return;
	}
	const auto enabled = obs_data_get_bool(request, "enabled");
	QString error;
	bool changed = false;
	const bool invoked = on_dock_thread([&](SrtlaDock *target) {
		changed = target->websocketSetLinkEnabled(linkId, enabled, error);
	});
	if (!invoked || !changed) {
		set_error(response, invoked ? error : QStringLiteral("SRTLA dock is unavailable"));
		return;
	}
	obs_data_set_bool(response, "success", true);
	obs_data_set_string(response, "linkId", linkIdText);
	obs_data_set_bool(response, "enabled", enabled);
}

void request_set_bitrate_control(obs_data_t *request, obs_data_t *response, void *)
{
	if (!obs_data_has_user_value(request, "automatic") || !obs_data_has_user_value(request, "manualBitrateKbps")) {
		set_error(response, "automatic and manualBitrateKbps are required");
		return;
	}
	const auto automatic = obs_data_get_bool(request, "automatic");
	const auto manual = obs_data_get_int(request, "manualBitrateKbps");
	if (manual < 0 || manual > std::numeric_limits<int>::max()) {
		set_error(response, "manualBitrateKbps is outside the supported range");
		return;
	}
	QString error;
	bool changed = false;
	const bool invoked = on_dock_thread([&](SrtlaDock *target) {
		changed = target->websocketSetBitrateControl(automatic, static_cast<int>(manual), error);
	});
	if (!invoked || !changed) {
		set_error(response, invoked ? error : QStringLiteral("SRTLA dock is unavailable"));
		return;
	}
	obs_data_set_bool(response, "success", true);
	obs_data_set_bool(response, "automatic", automatic);
	obs_data_set_int(response, "manualBitrateKbps", manual);
}

void request_set_max_bitrate(obs_data_t *request, obs_data_t *response, void *)
{
	if (!obs_data_has_user_value(request, "maxBitrateKbps")) {
		set_error(response, "maxBitrateKbps is required");
		return;
	}
	const auto maximum = obs_data_get_int(request, "maxBitrateKbps");
	if (maximum < 0 || maximum > std::numeric_limits<int>::max()) {
		set_error(response, "maxBitrateKbps is outside the supported range");
		return;
	}
	QString error;
	bool changed = false;
	const bool invoked = on_dock_thread([&](SrtlaDock *target) {
		changed = target->websocketSetMaxBitrate(static_cast<int>(maximum), error);
	});
	if (!invoked || !changed) {
		set_error(response, invoked ? error : QStringLiteral("SRTLA dock is unavailable"));
		return;
	}
	obs_data_set_bool(response, "success", true);
	obs_data_set_int(response, "maxBitrateKbps", maximum);
}

} // namespace

bool srtla_websocket_initialize(SrtlaDock *activeDock)
{
	dock.store(activeDock);
	calldata_t data{};
	if (!proc_handler_call(obs_get_proc_handler(), "obs_websocket_api_get_ph", &data)) {
		calldata_free(&data);
		blog(LOG_INFO, "obs-websocket is unavailable; SRTLA vendor requests are disabled");
		return false;
	}
	websocket_proc_handler = static_cast<proc_handler_t *>(calldata_ptr(&data, "ph"));
	calldata_free(&data);
	if (!websocket_proc_handler)
		return false;

	data = {};
	calldata_set_string(&data, "name", vendor_name);
	call_websocket_proc("vendor_register", &data);
	websocket_vendor = calldata_ptr(&data, "vendor");
	calldata_free(&data);
	if (!websocket_vendor)
		return false;

	const bool status = register_request("GetStatus", request_get_status);
	const bool output = register_request("SetOutputActive", request_set_output_active);
	const bool link = register_request("SetLinkEnabled", request_set_link_enabled);
	const bool bitrate = register_request("SetBitrateControl", request_set_bitrate_control);
	const bool maximum = register_request("SetMaxBitrate", request_set_max_bitrate);
	if (!status || !output || !link || !bitrate || !maximum) {
		blog(LOG_WARNING, "SRTLA obs-websocket vendor registered only partially");
		srtla_websocket_shutdown();
		return false;
	}
	blog(LOG_INFO, "SRTLA obs-websocket vendor requests registered");
	return true;
}

void srtla_websocket_shutdown()
{
	if (websocket_vendor) {
		unregister_request("GetStatus");
		unregister_request("SetOutputActive");
		unregister_request("SetLinkEnabled");
		unregister_request("SetBitrateControl");
		unregister_request("SetMaxBitrate");
	}
	dock.store(nullptr);
	websocket_vendor = nullptr;
	websocket_proc_handler = nullptr;
}

void srtla_websocket_emit_status_json(const char *json)
{
	if (!websocket_vendor || !json || !*json)
		return;
	auto *eventData = obs_data_create_from_json(json);
	if (!eventData)
		return;
	calldata_t data{};
	calldata_set_ptr(&data, "vendor", websocket_vendor);
	calldata_set_string(&data, "type", "StatusChanged");
	calldata_set_ptr(&data, "data", eventData);
	call_websocket_proc("vendor_event_emit", &data);
	calldata_free(&data);
	obs_data_release(eventData);
}

#include <obs-module.h>
#include <obs.h>
#include <obs-encoder.h>
#include <obs-data.h>
#include <obs-properties.h>
#include <obs-frontend-api.h>

#include "../engine/include/srtla_engine.h"
#include "mpegts-avformat-sink.hpp"
#include "network-monitor.hpp"
#include "output-capture-lifecycle.hpp"
#include "secret-store.hpp"
#include "srt-session.hpp"

#include <atomic>
#include <algorithm>
#include <chrono>
#include <condition_variable>
#include <cstdlib>
#include <cstdio>
#include <cstdint>
#include <deque>
#include <memory>
#include <mutex>
#include <string>
#include <stdexcept>
#include <thread>
#include <unordered_map>
#include <unordered_set>
#include <utility>
#include <vector>

struct QueuedTsDatagram {
	std::vector<std::uint8_t> bytes;
	bool keyframe = false;
	std::int64_t pts90k = 0;
};

struct SrtlaOutput {
	obs_output_t *output = nullptr;
	SrtlaEngineHandle *engine = nullptr;
	std::atomic_bool running{false};
	std::atomic<std::uint64_t> total_bytes{0};
	std::atomic<int> dropped_frames{0};
	std::mutex queue_mutex;
	std::condition_variable queue_changed;
	std::deque<QueuedTsDatagram> queue;
	std::uint64_t queue_bytes = 0;
	bool drop_until_keyframe = false;
	bool worker_stop = false;
	std::thread worker;
	std::atomic_bool abr_stop{false};
	std::thread abr_worker;
	std::mutex bitrate_mutex;
	std::atomic_bool needs_muxer_reset{false};
	std::atomic_bool capture_active{false};
	std::atomic_bool fatal_signal_queued{false};
	std::unique_ptr<OutputCaptureLifecycle> capture_lifecycle;
	std::unique_ptr<MpegTsAvformatSink> muxer;
	std::unique_ptr<SrtSession> session;
	std::string receiver_host;
	unsigned receiver_port = 0;
	std::string stream_id;
	std::string passphrase;
	bool credential_error = false;
	int latency_ms = 2000;
	int pbkeylen = 16;
	bool hevc = false;
	bool opus = false;
	std::atomic_bool auto_bitrate{true};
	bool shared_encoder = false;
	std::string encoder_source = "streaming";
	std::atomic<int> manual_bitrate_kbps{1500};
	int min_bitrate_kbps = 500;
	int max_bitrate_kbps = 100000;
	int start_bitrate_kbps = 1500;
	double safety_margin = 0.80;
	std::uint64_t audio_bitrate_bps = 128000;
	int original_bitrate_kbps = 0;
	bool bitrate_overridden = false;
};

static std::mutex output_registry_mutex;
static std::unordered_map<obs_output_t *, SrtlaOutput *> output_registry;

static std::string json_escape(const std::string &value)
{
	std::string escaped;
	escaped.reserve(value.size() + 8);
	for (const char c : value) {
		switch (c) {
		case '\\': escaped += "\\\\"; break;
		case '"': escaped += "\\\""; break;
		case '\n': escaped += "\\n"; break;
		case '\r': escaped += "\\r"; break;
		case '\t': escaped += "\\t"; break;
		default: escaped.push_back(c); break;
		}
	}
	return escaped;
}

static std::pair<std::string, unsigned> parse_endpoint(const char *url)
{
	std::string endpoint = (url && *url) ? url : "srtla://receiver.example:5000";
	const auto scheme = endpoint.find("://");
	if (scheme != std::string::npos)
		endpoint.erase(0, scheme + 3);
	// The form fields are the single source of truth.  Still accept a pasted
	// SRTLA URL by ignoring query/path suffixes here; the dock exposes those
	// values separately and never writes the passphrase back into a URL.
	if (const auto suffix = endpoint.find_first_of("?/"); suffix != std::string::npos)
		endpoint.erase(suffix);
	unsigned port = 5000;
	std::string host = endpoint;
	if (!host.empty() && host.front() == '[') {
		const auto close = host.find(']');
		if (close != std::string::npos) {
			const auto colon = host.find(':', close);
			if (colon != std::string::npos)
				port = static_cast<unsigned>(std::strtoul(host.c_str() + colon + 1, nullptr, 10));
			host = host.substr(1, close - 1);
		}
	} else if (const auto colon = host.rfind(':'); colon != std::string::npos) {
		port = static_cast<unsigned>(std::strtoul(host.c_str() + colon + 1, nullptr, 10));
		host.resize(colon);
	}
	if (host.empty() || host.size() > 240 || host.find_first_of("\\\"") != std::string::npos || port == 0 || port > 65535)
		return {"receiver.example", 5000};
	return {host, port};
}

static std::string url_decode_component(const std::string &value)
{
	std::string decoded;
	decoded.reserve(value.size());
	for (std::size_t i = 0; i < value.size(); ++i) {
		if (value[i] == '%' && i + 2 < value.size()) {
			const auto hex = [](char c) -> int {
				if (c >= '0' && c <= '9') return c - '0';
				if (c >= 'a' && c <= 'f') return c - 'a' + 10;
				if (c >= 'A' && c <= 'F') return c - 'A' + 10;
				return -1;
			};
			const int hi = hex(value[i + 1]);
			const int lo = hex(value[i + 2]);
			if (hi >= 0 && lo >= 0) {
				decoded.push_back(static_cast<char>((hi << 4) | lo));
				i += 2;
				continue;
			}
		}
		decoded.push_back(value[i] == '+' ? ' ' : value[i]);
	}
	return decoded;
}

static void split_url_query_into_settings(obs_data_t *settings)
{
	if (!settings)
		return;
	const char *raw = obs_data_get_string(settings, "url");
	if (!raw || !*raw)
		return;
	const std::string url(raw);
	const auto query_pos = url.find('?');
	if (query_pos == std::string::npos)
		return;
	const auto query = url.substr(query_pos + 1);
	std::size_t start = 0;
	while (start <= query.size()) {
		const auto end = query.find('&', start);
		const auto item = query.substr(start, end == std::string::npos ? std::string::npos : end - start);
		const auto equal = item.find('=');
		if (equal != std::string::npos) {
			const auto key = item.substr(0, equal);
			const auto value = url_decode_component(item.substr(equal + 1));
			if (key == "streamid" || key == "stream_id") obs_data_set_string(settings, "stream_id", value.c_str());
			else if (key == "passphrase" || key == "password") obs_data_set_string(settings, "passphrase", value.c_str());
			else if (key == "latency") obs_data_set_int(settings, "latency_ms", std::strtoll(value.c_str(), nullptr, 10));
			else if (key == "pbkeylen") obs_data_set_int(settings, "pbkeylen", std::strtoll(value.c_str(), nullptr, 10));
		}
		if (end == std::string::npos)
			break;
		start = end + 1;
	}
	// The individual fields are now the source of truth.  Remove secrets and
	// query material from the persisted URL so OBS never re-displays a password
	// in a copied URL or log line.
	obs_data_set_string(settings, "url", url.substr(0, query_pos).c_str());
}

static std::uint64_t stable_link_id(const std::string &value)
{
	std::uint64_t hash = 1469598103934665603ULL;
	for (const unsigned char c : value) {
		hash ^= c;
		hash *= 1099511628211ULL;
	}
	return hash;
}

static std::unordered_set<std::string> parse_enabled_links(const char *csv)
{
	std::unordered_set<std::string> enabled;
	if (!csv)
		return enabled;
	std::string value(csv);
	std::size_t start = 0;
	while (start <= value.size()) {
		const auto end = value.find(',', start);
		const auto token = value.substr(start, end == std::string::npos ? std::string::npos : end - start);
		if (!token.empty())
			enabled.insert(token);
		if (end == std::string::npos)
			break;
		start = end + 1;
	}
	return enabled;
}

static std::string enumerate_links_json(const std::unordered_set<std::string> *enabled_links = nullptr)
{
	std::string links;
	for (const auto &adapter : NetworkMonitor().enumerate()) {
		if (!links.empty())
			links += ',';
		links += "{\"id\":" + std::to_string(stable_link_id(adapter.id)) +
			",\"label\":\"" + json_escape(adapter.label + " / " + adapter.address) + "\",\"address\":\"" + json_escape(adapter.address) + "\",\"enabled\":" +
			(enabled_links && enabled_links->count(adapter.id) ? "true" : "false") + "}";
	}
	return "[" + links + "]";
}

static SrtlaEngineHandle *create_engine_from_settings(obs_data_t *settings)
{
	const auto endpoint = parse_endpoint(settings ? obs_data_get_string(settings, "url") : nullptr);
	const auto enabled_links = parse_enabled_links(settings ? obs_data_get_string(settings, "enabled_links") : nullptr);
	const std::string links = enumerate_links_json(&enabled_links);
	const char *stored_secret = settings ? obs_data_get_string(settings, "passphrase_dpapi") : nullptr;
	const char *plain_secret = settings ? obs_data_get_string(settings, "passphrase") : nullptr;
	const std::string passphrase = stored_secret && *stored_secret ? unprotect_srtla_secret(stored_secret) :
		(plain_secret ? std::string(plain_secret) : std::string());
	const char *stream_id = settings ? obs_data_get_string(settings, "stream_id") : nullptr;
	const auto latency = settings ? obs_data_get_int(settings, "latency_ms") : 2000;
	const auto pbkeylen = settings ? obs_data_get_int(settings, "pbkeylen") : 16;
	const char *scheduler = settings ? obs_data_get_string(settings, "scheduler") : nullptr;
	const auto min_bitrate = settings ? obs_data_get_int(settings, "min_bitrate") : 500;
	const auto start_bitrate = settings && obs_data_has_user_value(settings, "start_bitrate") ?
		obs_data_get_int(settings, "start_bitrate") : settings ? obs_data_get_int(settings, "bitrate") : 1500;
	const auto max_bitrate = settings ? obs_data_get_int(settings, "max_bitrate") : 100000;
	const auto safety_margin = settings ? obs_data_get_double(settings, "safety_margin") : 0.80;
	const auto audio_bitrate = settings ? obs_data_get_int(settings, "audio_bitrate") : 128;
	const std::string config = "{\"receiver_host\":\"" + json_escape(endpoint.first) +
		"\",\"receiver_port\":" + std::to_string(endpoint.second) +
		",\"stream_id\":\"" + json_escape(stream_id ? stream_id : "") +
		"\",\"latency_ms\":" + std::to_string(latency) +
		",\"passphrase\":\"" + json_escape(passphrase) +
		"\",\"pbkeylen\":" + std::to_string(pbkeylen) +
		",\"scheduler\":\"" + json_escape(scheduler ? scheduler : "enhanced") +
		"\",\"links\":" + links + ",\"abr\":{\"safety_margin\":" + std::to_string(safety_margin) +
		",\"min_bps\":" + std::to_string(std::max<std::int64_t>(1, min_bitrate) * 1000) +
		",\"start_bps\":" + std::to_string(std::max<std::int64_t>(1, start_bitrate) * 1000) +
		",\"max_bps\":" + std::to_string(std::max<std::int64_t>(1, max_bitrate) * 1000) +
		"},\"audio_bps\":" + std::to_string(std::max<std::int64_t>(0, audio_bitrate) * 1000) + "}";
	return srtla_engine_create_v1(config.c_str());
}

static const char *srtla_output_name(void *)
{
	return "SRTLA (native)";
}

static void *srtla_output_create(obs_data_t *settings, obs_output_t *output)
{
	split_url_query_into_settings(settings);
	bool secret_error = false;
	if (settings) {
		const char *plain = obs_data_get_string(settings, "passphrase");
		if (plain && *plain) {
			const auto protected_secret = protect_srtla_secret(plain);
			if (!protected_secret.empty()) {
				obs_data_set_string(settings, "passphrase_dpapi", protected_secret.c_str());
				obs_data_set_string(settings, "passphrase", "");
			} else {
				secret_error = true;
				obs_data_set_string(settings, "passphrase", "");
			}
		}
	}
	auto *data = new SrtlaOutput();
	data->output = output;
	data->hevc = settings && obs_data_get_string(settings, "video_codec") &&
		std::string(obs_data_get_string(settings, "video_codec")) == "hevc";
	data->opus = settings && obs_data_get_string(settings, "audio_codec") &&
		std::string(obs_data_get_string(settings, "audio_codec")) == "opus";
	data->auto_bitrate.store(!settings || !obs_data_has_user_value(settings, "auto_bitrate") || obs_data_get_bool(settings, "auto_bitrate"));
	const char *encoder_mode = settings ? obs_data_get_string(settings, "encoder_mode") : nullptr;
	data->shared_encoder = encoder_mode && std::string(encoder_mode) == "shared";
	const char *encoder_source = settings ? obs_data_get_string(settings, "encoder_source") : nullptr;
	data->encoder_source = encoder_source && *encoder_source ? encoder_source : "streaming";
	data->manual_bitrate_kbps.store(settings && obs_data_has_user_value(settings, "bitrate") ? static_cast<int>(obs_data_get_int(settings, "bitrate")) : 1500);
	data->min_bitrate_kbps = settings ? static_cast<int>(obs_data_get_int(settings, "min_bitrate")) : 500;
	data->max_bitrate_kbps = settings ? static_cast<int>(obs_data_get_int(settings, "max_bitrate")) : 100000;
	data->start_bitrate_kbps = settings && obs_data_has_user_value(settings, "start_bitrate") ?
		static_cast<int>(obs_data_get_int(settings, "start_bitrate")) : data->manual_bitrate_kbps.load();
	data->safety_margin = settings ? obs_data_get_double(settings, "safety_margin") : 0.80;
	data->audio_bitrate_bps = settings ? static_cast<std::uint64_t>(std::max<std::int64_t>(0, obs_data_get_int(settings, "audio_bitrate"))) * 1000ULL : 128000ULL;
	const auto endpoint = parse_endpoint(settings ? obs_data_get_string(settings, "url") : nullptr);
	data->receiver_host = endpoint.first;
	data->receiver_port = endpoint.second;
	data->stream_id = settings && obs_data_get_string(settings, "stream_id") ? obs_data_get_string(settings, "stream_id") : "";
	const char *stored_secret = settings ? obs_data_get_string(settings, "passphrase_dpapi") : nullptr;
	const char *plain_secret = settings ? obs_data_get_string(settings, "passphrase") : nullptr;
	if (stored_secret && *stored_secret) {
		data->passphrase = unprotect_srtla_secret(stored_secret);
		secret_error = secret_error || data->passphrase.empty();
	} else {
		data->passphrase = plain_secret ? plain_secret : "";
	}
	data->credential_error = secret_error;
	data->latency_ms = settings ? std::clamp(static_cast<int>(obs_data_get_int(settings, "latency_ms")), 120, 60000) : 2000;
	data->pbkeylen = settings ? static_cast<int>(obs_data_get_int(settings, "pbkeylen")) : 16;
	if (data->pbkeylen != 16 && data->pbkeylen != 24 && data->pbkeylen != 32) data->pbkeylen = 16;
	data->engine = create_engine_from_settings(settings);
	{
		std::lock_guard<std::mutex> lock(output_registry_mutex);
		output_registry.emplace(output, data);
	}
	return data;
}

static void srtla_output_update(void *opaque, obs_data_t *settings)
{
	auto *data = static_cast<SrtlaOutput *>(opaque);
	if (data->running.load())
		return;
	data->credential_error = false;
	if (settings) {
		split_url_query_into_settings(settings);
		const char *plain = obs_data_get_string(settings, "passphrase");
		if (plain && *plain) {
			const auto protected_secret = protect_srtla_secret(plain);
			if (!protected_secret.empty()) {
				obs_data_set_string(settings, "passphrase_dpapi", protected_secret.c_str());
				obs_data_set_string(settings, "passphrase", "");
			} else {
				data->credential_error = true;
				obs_data_set_string(settings, "passphrase", "");
			}
		}
	}
	if (data->engine)
		srtla_engine_destroy(data->engine);
	data->engine = create_engine_from_settings(settings);
	const char *video_codec = settings ? obs_data_get_string(settings, "video_codec") : nullptr;
	const char *audio_codec = settings ? obs_data_get_string(settings, "audio_codec") : nullptr;
	data->hevc = video_codec && std::string(video_codec) == "hevc";
	data->opus = audio_codec && std::string(audio_codec) == "opus";
	data->auto_bitrate.store(!settings || !obs_data_has_user_value(settings, "auto_bitrate") || obs_data_get_bool(settings, "auto_bitrate"));
	const char *encoder_mode = settings ? obs_data_get_string(settings, "encoder_mode") : nullptr;
	data->shared_encoder = encoder_mode && std::string(encoder_mode) == "shared";
	const char *encoder_source = settings ? obs_data_get_string(settings, "encoder_source") : nullptr;
	data->encoder_source = encoder_source && *encoder_source ? encoder_source : "streaming";
	data->manual_bitrate_kbps.store(settings && obs_data_has_user_value(settings, "bitrate") ? static_cast<int>(obs_data_get_int(settings, "bitrate")) : 1500);
	data->min_bitrate_kbps = settings ? static_cast<int>(obs_data_get_int(settings, "min_bitrate")) : 500;
	data->max_bitrate_kbps = settings ? static_cast<int>(obs_data_get_int(settings, "max_bitrate")) : 100000;
	data->start_bitrate_kbps = settings && obs_data_has_user_value(settings, "start_bitrate") ?
		static_cast<int>(obs_data_get_int(settings, "start_bitrate")) : data->manual_bitrate_kbps.load();
	data->safety_margin = settings ? obs_data_get_double(settings, "safety_margin") : 0.80;
	data->audio_bitrate_bps = settings ? static_cast<std::uint64_t>(std::max<std::int64_t>(0, obs_data_get_int(settings, "audio_bitrate"))) * 1000ULL : 128000ULL;
	const auto endpoint = parse_endpoint(settings ? obs_data_get_string(settings, "url") : nullptr);
	data->receiver_host = endpoint.first;
	data->receiver_port = endpoint.second;
	data->stream_id = settings && obs_data_get_string(settings, "stream_id") ? obs_data_get_string(settings, "stream_id") : "";
	const char *stored_secret = settings ? obs_data_get_string(settings, "passphrase_dpapi") : nullptr;
	const char *plain_secret = settings ? obs_data_get_string(settings, "passphrase") : nullptr;
	if (stored_secret && *stored_secret) {
		data->passphrase = unprotect_srtla_secret(stored_secret);
		data->credential_error = data->credential_error || data->passphrase.empty();
	} else {
		data->passphrase = plain_secret ? plain_secret : "";
	}
	data->latency_ms = settings ? std::clamp(static_cast<int>(obs_data_get_int(settings, "latency_ms")), 120, 60000) : 2000;
	data->pbkeylen = settings ? static_cast<int>(obs_data_get_int(settings, "pbkeylen")) : 16;
	if (data->pbkeylen != 16 && data->pbkeylen != 24 && data->pbkeylen != 32) data->pbkeylen = 16;
}

static void restore_shared_bitrate(SrtlaOutput *data)
{
	if (!data->output)
		return;
	std::lock_guard<std::mutex> bitrate_lock(data->bitrate_mutex);
	if (!data->bitrate_overridden)
		return;
	if (auto *encoder = obs_output_get_video_encoder(data->output)) {
		if (auto *settings = obs_encoder_get_settings(encoder)) {
			obs_data_set_int(settings, "bitrate", data->original_bitrate_kbps);
			obs_encoder_update(encoder, settings);
			obs_data_release(settings);
		}
	}
	data->bitrate_overridden = false;
}

static void apply_manual_bitrate(SrtlaOutput *data)
{
	if (!data->output)
		return;
	std::lock_guard<std::mutex> bitrate_lock(data->bitrate_mutex);
	if (auto *encoder = obs_output_get_video_encoder(data->output)) {
		if (auto *settings = obs_encoder_get_settings(encoder)) {
			if (!data->bitrate_overridden)
				data->original_bitrate_kbps = static_cast<int>(obs_data_get_int(settings, "bitrate"));
			if (!data->auto_bitrate.load()) {
				obs_data_set_int(settings, "bitrate", data->manual_bitrate_kbps.load());
				obs_encoder_update(encoder, settings);
			}
			obs_data_release(settings);
			data->bitrate_overridden = true;
		}
	}
}

static void stop_worker(SrtlaOutput *data)
{
	{
		std::lock_guard<std::mutex> lock(data->queue_mutex);
		data->worker_stop = true;
		data->queue.clear();
		data->queue_bytes = 0;
	}
	data->queue_changed.notify_all();
	if (data->worker.joinable())
		data->worker.join();
}

static void stop_abr_worker(SrtlaOutput *data)
{
	data->abr_stop.store(true);
	if (data->abr_worker.joinable())
		data->abr_worker.join();
}

static void queue_fatal_output(SrtlaOutput *data, const std::string &error);

static void start_abr_worker(SrtlaOutput *data)
{
	data->abr_stop.store(false);
	data->abr_worker = std::thread([data] {
		while (!data->abr_stop.load()) {
			for (int i = 0; i < 10 && !data->abr_stop.load(); ++i)
				std::this_thread::sleep_for(std::chrono::milliseconds(100));
			if (data->abr_stop.load() || !data->running.load() || !data->engine)
				continue;
			if (const char *engine_error = srtla_engine_last_error(data->engine);
			    engine_error && *engine_error) {
				queue_fatal_output(data, engine_error);
				continue;
			}
			const auto now = static_cast<std::uint64_t>(std::chrono::duration_cast<std::chrono::milliseconds>(
				std::chrono::system_clock::now().time_since_epoch()).count());
			SrtlaSrtStatsV1 srt_stats{};
			if (data->session && data->session->sample_stats(now, srt_stats))
				(void)srtla_engine_update_srt_stats(data->engine, &srt_stats);
			const auto applied_bps = srtla_engine_apply_abr(data->engine, now);
			if (!data->auto_bitrate.load()) {
				std::lock_guard<std::mutex> bitrate_lock(data->bitrate_mutex);
				if (data->auto_bitrate.load())
					continue;
				// Keep calculating the capacity recommendation for diagnostics,
				// but report the bitrate the manual encoder is actually using.
				// apply_abr updates the engine's current-video field even when the
				// OBS encoder update below is intentionally bypassed.
				(void)srtla_engine_set_video_bitrate(data->engine,
					static_cast<std::uint64_t>(std::max(1, data->manual_bitrate_kbps.load())) * 1000ULL);
				continue;
			}
			if (applied_bps == 0 || !data->output)
				continue;
			std::lock_guard<std::mutex> bitrate_lock(data->bitrate_mutex);
			if (!data->auto_bitrate.load())
				continue;
			if (auto *encoder = obs_output_get_video_encoder(data->output)) {
				if (!(obs_encoder_get_caps(encoder) & OBS_ENCODER_CAP_DYN_BITRATE))
					continue;
				if (auto *settings = obs_encoder_get_settings(encoder)) {
					const auto target_kbps = static_cast<int>(applied_bps / 1000);
					if (target_kbps > 0 && obs_data_get_int(settings, "bitrate") != target_kbps) {
						obs_data_set_int(settings, "bitrate", target_kbps);
						obs_encoder_update(encoder, settings);
					}
					obs_data_release(settings);
				}
			}
			(void)srtla_engine_set_video_bitrate(data->engine, applied_bps);
		}
	});
}

static bool supported_video_encoder(obs_encoder_t *encoder)
{
	const char *codec = encoder ? obs_encoder_get_codec(encoder) : nullptr;
	return codec && (strcmp(codec, "h264") == 0 || strcmp(codec, "hevc") == 0);
}

static bool supported_audio_encoder(obs_encoder_t *encoder)
{
	const char *codec = encoder ? obs_encoder_get_codec(encoder) : nullptr;
	return !encoder || (codec && (strcmp(codec, "aac") == 0 || strcmp(codec, "opus") == 0));
}

struct FatalOutputTask {
	obs_output_t *output = nullptr;
	SrtlaOutput *data = nullptr;
	std::string error;
};

static void srtla_output_stop(void *opaque, uint64_t ts);

static void signal_fatal_output_on_ui(void *opaque)
{
	std::unique_ptr<FatalOutputTask> task(static_cast<FatalOutputTask *>(opaque));
	if (!task || !task->output)
		return;
	if (!task->data || !task->data->running.load()) {
		obs_output_release(task->output);
		return;
	}
	obs_output_set_last_error(task->output, task->error.empty() ? "SRT session failed" : task->error.c_str());
	obs_output_signal_stop(task->output, OBS_OUTPUT_ERROR);
	// obs_output_signal_stop ends the encoder capture, but output-specific
	// resources are released by the output's stop callback.  Invoke that
	// callback on this UI task (never from the connector thread) so the engine,
	// session and workers cannot survive a fatal local error.
	if (task->data)
		srtla_output_stop(task->data, 0);
	obs_output_release(task->output);
}

static void queue_fatal_output(SrtlaOutput *data, const std::string &error)
{
	if (!data || !data->running.load() || data->fatal_signal_queued.exchange(true))
		return;
	auto *output = obs_output_get_ref(data->output);
	if (!output) {
		data->fatal_signal_queued.store(false);
		return;
	}
	try {
		auto *task = new FatalOutputTask{output, data, error};
		obs_queue_task(OBS_TASK_UI, signal_fatal_output_on_ui, task, false);
	} catch (...) {
		data->fatal_signal_queued.store(false);
		obs_output_release(output);
	}
}

static void on_session_status(SrtlaOutput *data, SrtSession::Status status, const std::string &error)
{
	if (!data)
		return;
	if (status == SrtSession::Status::Fatal && data->running.load()) {
		queue_fatal_output(data, error);
		return;
	}
	if (status != SrtSession::Status::Reconnecting)
		return;
	{
		std::lock_guard<std::mutex> lock(data->queue_mutex);
		data->queue.clear();
		data->queue_bytes = 0;
		data->drop_until_keyframe = true;
	}
	data->needs_muxer_reset.store(true);
	data->queue_changed.notify_all();
}

static void enqueue_datagram(SrtlaOutput *data, std::vector<std::uint8_t> bytes, bool keyframe,
	                         std::int64_t pts90k)
{
	if (!data->running.load() || bytes.empty())
		return;
	std::lock_guard<std::mutex> lock(data->queue_mutex);
	constexpr std::uint64_t max_queue_bytes = 8ULL * 1024ULL * 1024ULL;
	constexpr std::int64_t max_queue_duration = 2 * 90000;
	if (data->drop_until_keyframe && !keyframe) {
		data->dropped_frames.fetch_add(1);
		return;
	}
	if (keyframe && data->drop_until_keyframe) {
		data->queue.clear();
		data->queue_bytes = 0;
		data->drop_until_keyframe = false;
	}
	const auto duration = data->queue.empty() ? 0 : pts90k - data->queue.front().pts90k;
	if (data->queue_bytes + bytes.size() > max_queue_bytes || duration > max_queue_duration) {
		data->drop_until_keyframe = true;
		data->needs_muxer_reset.store(true);
		data->dropped_frames.fetch_add(1);
		return;
	}
	data->total_bytes.fetch_add(bytes.size());
	data->queue_bytes += bytes.size();
	data->queue.push_back(QueuedTsDatagram{std::move(bytes), keyframe, pts90k});
	data->queue_changed.notify_one();
}

static void start_worker(SrtlaOutput *data)
{
	data->worker_stop = false;
	data->worker = std::thread([data] {
		for (;;) {
			QueuedTsDatagram item;
			{
				std::unique_lock<std::mutex> lock(data->queue_mutex);
				data->queue_changed.wait(lock, [data] { return data->worker_stop || !data->queue.empty(); });
				if (data->queue.empty() && data->worker_stop)
					return;
				item = std::move(data->queue.front());
				data->queue.pop_front();
				data->queue_bytes -= item.bytes.size();
			}
			if (!data->session || !data->session->send_ts(item.bytes.data(), item.bytes.size())) {
				{
					std::lock_guard<std::mutex> lock(data->queue_mutex);
					if (data->worker_stop)
						return;
					// A reconnect must resume at a fresh MPEG-TS table/keyframe
					// boundary; retaining old datagrams would extend the outage and
					// violate the bounded two-second queue contract.
					data->queue.clear();
					data->queue_bytes = 0;
					data->dropped_frames.fetch_add(1);
					data->drop_until_keyframe = true;
				}
				data->needs_muxer_reset.store(true);
				std::this_thread::sleep_for(std::chrono::milliseconds(10));
			}
		}
	});
}

static void srtla_output_destroy(void *opaque)
{
	auto *data = static_cast<SrtlaOutput *>(opaque);
	data->running.store(false);
	data->fatal_signal_queued.store(false);
	if (data->capture_lifecycle)
		data->capture_lifecycle->end();
	data->capture_active.store(false);
	stop_abr_worker(data);
	stop_worker(data);
	if (data->session)
		data->session->stop();
	restore_shared_bitrate(data);
	data->muxer.reset();
	{
		std::lock_guard<std::mutex> lock(output_registry_mutex);
		output_registry.erase(data->output);
	}
	if (data->engine)
		srtla_engine_destroy(data->engine);
	delete data;
}

extern "C" std::uint64_t srtla_output_link_id(const char *adapter_id)
{
	return stable_link_id(adapter_id ? std::string(adapter_id) : std::string());
}

extern "C" int srtla_output_set_link_enabled(obs_output_t *output, std::uint64_t link_id, bool enabled)
{
	std::lock_guard<std::mutex> lock(output_registry_mutex);
	const auto it = output_registry.find(output);
	if (it == output_registry.end() || !it->second->engine)
		return -1;
	return srtla_engine_set_link_enabled(it->second->engine, link_id, enabled);
}

extern "C" int srtla_output_set_bitrate_control(obs_output_t *output, bool automatic,
	                                             int manual_bitrate_kbps)
{
	std::lock_guard<std::mutex> registry_lock(output_registry_mutex);
	const auto it = output_registry.find(output);
	if (it == output_registry.end() || !it->second->engine)
		return -1;

	auto *data = it->second;
	std::lock_guard<std::mutex> bitrate_lock(data->bitrate_mutex);
	const int minimum = std::max(1, data->min_bitrate_kbps);
	const int maximum = std::max(minimum, data->max_bitrate_kbps);
	const int manual = std::clamp(manual_bitrate_kbps, minimum, maximum);
	auto *encoder = data->output ? obs_output_get_video_encoder(data->output) : nullptr;
	if (data->running.load() &&
	    (!encoder || !(obs_encoder_get_caps(encoder) & OBS_ENCODER_CAP_DYN_BITRATE)))
		return -2;

	int active_bitrate_kbps = manual;
	if (encoder) {
		auto *settings = obs_encoder_get_settings(encoder);
		if (!settings)
			return -3;
		if (automatic) {
			active_bitrate_kbps = static_cast<int>(obs_data_get_int(settings, "bitrate"));
		} else if (obs_data_get_int(settings, "bitrate") != manual) {
			obs_data_set_int(settings, "bitrate", manual);
			obs_encoder_update(encoder, settings);
		}
		obs_data_release(settings);
	}

	data->manual_bitrate_kbps.store(manual);
	data->auto_bitrate.store(automatic);
	return srtla_engine_set_video_bitrate(
		data->engine, static_cast<std::uint64_t>(std::max(1, active_bitrate_kbps)) * 1000ULL);
}

extern "C" int srtla_output_set_max_bitrate(obs_output_t *output, int max_bitrate_kbps)
{
	std::lock_guard<std::mutex> registry_lock(output_registry_mutex);
	const auto it = output_registry.find(output);
	if (it == output_registry.end() || !it->second->engine)
		return -1;

	auto *data = it->second;
	std::lock_guard<std::mutex> bitrate_lock(data->bitrate_mutex);
	const int maximum = std::max(std::max(1, data->min_bitrate_kbps), max_bitrate_kbps);
	const bool apply_to_encoder = data->running.load() && data->auto_bitrate.load();
	auto *encoder = data->output ? obs_output_get_video_encoder(data->output) : nullptr;
	obs_data_t *encoder_settings = nullptr;
	int active_bitrate_kbps = maximum;

	if (apply_to_encoder) {
		if (!encoder || !(obs_encoder_get_caps(encoder) & OBS_ENCODER_CAP_DYN_BITRATE))
			return -2;
		encoder_settings = obs_encoder_get_settings(encoder);
		if (!encoder_settings)
			return -3;
		active_bitrate_kbps = static_cast<int>(obs_data_get_int(encoder_settings, "bitrate"));
		if (active_bitrate_kbps <= 0)
			active_bitrate_kbps = maximum;
	}

	const int result = srtla_engine_set_max_video_bitrate(
		data->engine, static_cast<std::uint64_t>(maximum) * 1000ULL);
	if (result != 0) {
		if (encoder_settings)
			obs_data_release(encoder_settings);
		return result;
	}
	data->max_bitrate_kbps = maximum;

	if (encoder_settings) {
		active_bitrate_kbps = std::min(active_bitrate_kbps, maximum);
		if (obs_data_get_int(encoder_settings, "bitrate") != active_bitrate_kbps) {
			obs_data_set_int(encoder_settings, "bitrate", active_bitrate_kbps);
			obs_encoder_update(encoder, encoder_settings);
		}
		obs_data_release(encoder_settings);
		return srtla_engine_set_video_bitrate(
			data->engine, static_cast<std::uint64_t>(active_bitrate_kbps) * 1000ULL);
	}
	return 0;
}

extern "C" int srtla_output_update_adapters(obs_output_t *output)
{
	std::lock_guard<std::mutex> lock(output_registry_mutex);
	const auto it = output_registry.find(output);
	if (it == output_registry.end() || !it->second->engine)
		return -1;
	const auto json = enumerate_links_json();
	return srtla_engine_update_adapters(it->second->engine, json.c_str());
}

extern "C" std::size_t srtla_output_copy_stats_json(obs_output_t *output, char *buffer, std::size_t capacity)
{
	std::lock_guard<std::mutex> lock(output_registry_mutex);
	const auto it = output_registry.find(output);
	if (it == output_registry.end() || !it->second->engine)
		return 0;
	return srtla_engine_copy_stats_json(it->second->engine, buffer, capacity);
}

static bool srtla_output_start(void *opaque)
{
	auto *data = static_cast<SrtlaOutput *>(opaque);
	if (!data->output || !supported_video_encoder(obs_output_get_video_encoder(data->output)) ||
	    !supported_audio_encoder(obs_output_get_audio_encoder(data->output, 0))) {
		if (data->output)
			obs_output_set_last_error(data->output, "Unsupported or missing OBS encoder");
		return false;
	}
	if (data->credential_error) {
		obs_output_set_last_error(data->output, "SRT passphrase could not be protected or decrypted");
		return false;
	}
	std::string capture_error;
	data->capture_lifecycle = std::make_unique<OutputCaptureLifecycle>(
		[data] { return obs_output_can_begin_data_capture(data->output, 0); },
		[data] { return obs_output_initialize_encoders(data->output, 0); },
		[data] { return obs_output_begin_data_capture(data->output, 0); },
		[data] { obs_output_end_data_capture(data->output); }, OutputCaptureLifecycle::StopSignal{},
		[data] { (void)obs_output_can_begin_data_capture(data->output, 0); });
	if (!data->capture_lifecycle->prepare(capture_error)) {
		obs_output_set_last_error(data->output, capture_error.c_str());
		return false;
	}
	data->fatal_signal_queued.store(false);
	if (data->shared_encoder) {
		obs_output_t *source_output = data->encoder_source == "recording" ? obs_frontend_get_recording_output() :
			obs_frontend_get_streaming_output();
		if (!source_output) {
			obs_output_set_last_error(data->output, "The selected shared OBS output is unavailable");
			return false;
		}
		const bool busy = source_output != data->output && obs_output_active(source_output);
		obs_output_release(source_output);
		if (busy) {
			obs_output_set_last_error(data->output, "The selected shared OBS encoder is already active");
			return false;
		}
	}
	// OBS has joined any previous end-data-capture worker by this point, so a
	// stopped muxer can be safely released before the next session.
	data->muxer.reset();
	data->capture_active.store(false);
	data->running.store(false);
	if (!data->engine) {
		obs_output_set_last_error(data->output, "SRTLA engine initialization failed");
		return false;
	}
	if (srtla_engine_start(data->engine) != 0) {
		const char *engine_error = srtla_engine_last_error(data->engine);
		obs_output_set_last_error(data->output, engine_error && *engine_error ? engine_error : "SRTLA engine start failed");
		return false;
	}
	if (srtla_engine_set_audio_bitrate(data->engine, data->audio_bitrate_bps) != 0) {
		obs_output_set_last_error(data->output, "SRTLA audio bitrate initialization failed");
		srtla_engine_stop(data->engine);
		return false;
	}
	try {
		data->session = std::make_unique<SrtSession>(data->engine, data->receiver_host,
			static_cast<std::uint16_t>(data->receiver_port), data->stream_id, data->passphrase,
			data->latency_ms, data->pbkeylen,
			[data](SrtSession::Status status, const std::string &error) { on_session_status(data, status, error); });
	} catch (...) {
		obs_output_set_last_error(data->output, "SRT session allocation failed");
		srtla_engine_stop(data->engine);
		return false;
	}
	if (!data->session->start() || !data->session->wait_connected(5000)) {
		const auto error = data->session->last_error().empty() ?
			"SRT receiver did not connect within five seconds" : data->session->last_error();
		obs_output_set_last_error(data->output, error.c_str());
		data->session->stop();
		data->session.reset();
		srtla_engine_stop(data->engine);
		return false;
	}
	if (data->auto_bitrate.load()) {
		if (auto *encoder = obs_output_get_video_encoder(data->output);
		    encoder && !(obs_encoder_get_caps(encoder) & OBS_ENCODER_CAP_DYN_BITRATE)) {
			// Hardware encoders without dynamic-bitrate support stay usable in
			// fixed mode; the dock still shows the recommendation as telemetry.
			data->auto_bitrate.store(false);
		}
	}
	apply_manual_bitrate(data);
	int active_bitrate_kbps = data->auto_bitrate.load() ?
		data->start_bitrate_kbps : data->manual_bitrate_kbps.load();
	if (data->auto_bitrate.load()) {
		std::lock_guard<std::mutex> bitrate_lock(data->bitrate_mutex);
		if (auto *encoder = obs_output_get_video_encoder(data->output)) {
			if (auto *encoder_settings = obs_encoder_get_settings(encoder)) {
				const auto encoder_bitrate = obs_data_get_int(encoder_settings, "bitrate");
				if (encoder_bitrate > 0)
					active_bitrate_kbps = static_cast<int>(encoder_bitrate);
				obs_data_release(encoder_settings);
			}
		}
	}
	if (srtla_engine_set_video_bitrate(
		data->engine, static_cast<std::uint64_t>(std::max(1, active_bitrate_kbps)) * 1000ULL) != 0) {
		obs_output_set_last_error(data->output, "SRTLA video bitrate initialization failed");
		data->session->stop();
		data->session.reset();
		srtla_engine_stop(data->engine);
		restore_shared_bitrate(data);
		return false;
	}
	{
		std::lock_guard<std::mutex> lock(data->queue_mutex);
		data->queue.clear();
		data->drop_until_keyframe = false;
		data->needs_muxer_reset.store(false);
		data->worker_stop = false;
	}
	try {
		data->muxer = std::make_unique<MpegTsAvformatSink>(data->output,
			[data](std::vector<std::uint8_t> bytes, bool keyframe, std::int64_t pts90k) {
				enqueue_datagram(data, std::move(bytes), keyframe, pts90k);
			});
	} catch (...) {
		obs_output_set_last_error(data->output, "MPEG-TS muxer allocation failed");
		data->session->stop();
		data->session.reset();
		srtla_engine_stop(data->engine);
		restore_shared_bitrate(data);
		return false;
	}
	data->running.store(true);
	try {
		start_worker(data);
		if (!data->capture_lifecycle->initialize_and_begin(capture_error))
			throw std::runtime_error(capture_error);
		data->capture_active.store(data->capture_lifecycle->active());
		start_abr_worker(data);
	} catch (...) {
		obs_output_set_last_error(data->output,
		                         capture_error.empty() ? "OBS output worker startup failed" : capture_error.c_str());
		data->running.store(false);
		if (data->capture_lifecycle)
			data->capture_lifecycle->end();
		data->capture_active.store(false);
		stop_abr_worker(data);
		stop_worker(data);
		if (data->session)
			data->session->stop();
		if (data->engine)
			srtla_engine_stop(data->engine);
		restore_shared_bitrate(data);
		data->muxer.reset();
		return false;
	}
	return true;
}

static void srtla_output_stop(void *opaque, uint64_t ts)
{
	UNUSED_PARAMETER(ts);
	auto *data = static_cast<SrtlaOutput *>(opaque);
	data->running.store(false);
	data->fatal_signal_queued.store(false);
	if (data->capture_lifecycle)
		data->capture_lifecycle->end();
	data->capture_active.store(false);
	stop_abr_worker(data);
	stop_worker(data);
	if (data->session)
		data->session->stop();
	if (data->engine)
		srtla_engine_stop(data->engine);
	restore_shared_bitrate(data);
	data->muxer.reset();
	std::lock_guard<std::mutex> lock(data->queue_mutex);
	data->queue.clear();
	data->queue_bytes = 0;
}

static void srtla_output_encoded_packet(void *opaque, struct encoder_packet *packet)
{
	auto *data = static_cast<SrtlaOutput *>(opaque);
	if (!data->running.load() || !packet || !packet->data || packet->size == 0)
		return;
	// After a reconnect the bounded queue drops everything until the next
	// keyframe.  Do not let an intervening delta frame mark PAT/PMT as already
	// emitted; otherwise the first accepted keyframe would lack fresh tables.
	if (data->needs_muxer_reset.load() && !packet->keyframe)
		return;
	if (data->needs_muxer_reset.exchange(false) && data->muxer)
		data->muxer->reset();
	if (!data->muxer || !data->muxer->write(packet)) {
		const std::string error = data->muxer && !data->muxer->last_error().empty() ?
			data->muxer->last_error() : "MPEG-TS muxer rejected an encoded packet";
		obs_output_set_last_error(data->output, error.c_str());
		queue_fatal_output(data, error);
		return;
	}
	// Flush at each callback.  A full 1316-byte datagram is emitted whenever
	// possible; a smaller final datagram bounds latency when the stream is idle.
	data->muxer->flush();
	if (!data->muxer->healthy())
		queue_fatal_output(data, data->muxer->last_error());
}

static void srtla_output_defaults(obs_data_t *settings)
{
	obs_data_set_default_string(settings, "url", "srtla://receiver.example:5000");
	obs_data_set_default_string(settings, "video_codec", "h264");
	obs_data_set_default_string(settings, "audio_codec", "aac");
	obs_data_set_default_int(settings, "latency_ms", 2000);
	obs_data_set_default_int(settings, "bitrate", 1500);
	obs_data_set_default_int(settings, "min_bitrate", 500);
	obs_data_set_default_int(settings, "max_bitrate", 100000);
	obs_data_set_default_int(settings, "audio_bitrate", 128);
	obs_data_set_default_double(settings, "safety_margin", 0.80);
	obs_data_set_default_bool(settings, "auto_bitrate", true);
	obs_data_set_default_string(settings, "encoder_mode", "dedicated");
	obs_data_set_default_string(settings, "encoder_source", "streaming");
	obs_data_set_default_string(settings, "audio_mix", "track_1");
	obs_data_set_default_int(settings, "pbkeylen", 16);
	obs_data_set_default_string(settings, "scheduler", "enhanced");
}

static obs_properties_t *srtla_output_properties(void *)
{
	auto *props = obs_properties_create();
	obs_properties_add_text(props, "url", "SRTLA URL", OBS_TEXT_DEFAULT);
	obs_properties_add_text(props, "stream_id", "Stream ID", OBS_TEXT_DEFAULT);
	auto *video_codec = obs_properties_add_list(props, "video_codec", "Video codec", OBS_COMBO_TYPE_LIST, OBS_COMBO_FORMAT_STRING);
	obs_property_list_add_string(video_codec, "H.264", "h264");
	obs_property_list_add_string(video_codec, "HEVC", "hevc");
	auto *audio_codec = obs_properties_add_list(props, "audio_codec", "Audio codec", OBS_COMBO_TYPE_LIST, OBS_COMBO_FORMAT_STRING);
	obs_property_list_add_string(audio_codec, "AAC", "aac");
	obs_property_list_add_string(audio_codec, "Opus", "opus");
	obs_properties_add_int(props, "latency_ms", "Latency (ms)", 120, 60'000, 10);
	obs_properties_add_int(props, "bitrate", "Video bitrate (kb/s)", 500, 100'000, 50);
	obs_properties_add_int(props, "min_bitrate", "Minimum bitrate (kb/s)", 500, 100'000, 50);
	obs_properties_add_int(props, "max_bitrate", "Maximum bitrate (kb/s)", 500, 200'000, 50);
	obs_properties_add_int(props, "audio_bitrate", "Audio bitrate (kb/s)", 32, 512, 8);
	obs_properties_add_float(props, "safety_margin", "Safety margin", 0.50, 0.95, 0.01);
	obs_properties_add_bool(props, "auto_bitrate", "Automatic bitrate");
	auto *encoder_source = obs_properties_add_list(props, "encoder_source", "Encoder source", OBS_COMBO_TYPE_LIST, OBS_COMBO_FORMAT_STRING);
	obs_property_list_add_string(encoder_source, "Streaming", "streaming");
	obs_property_list_add_string(encoder_source, "Recording", "recording");
	obs_property_list_add_string(encoder_source, "Custom", "custom");
	auto *encoder_mode = obs_properties_add_list(props, "encoder_mode", "Encoder mode", OBS_COMBO_TYPE_LIST, OBS_COMBO_FORMAT_STRING);
	obs_property_list_add_string(encoder_mode, "Dedicated", "dedicated");
	obs_property_list_add_string(encoder_mode, "Shared", "shared");
	auto *audio_mix = obs_properties_add_list(props, "audio_mix", "Audio mix", OBS_COMBO_TYPE_LIST, OBS_COMBO_FORMAT_STRING);
	obs_property_list_add_string(audio_mix, "Track 1", "track_1");
	obs_properties_add_text(props, "passphrase", "Passphrase", OBS_TEXT_PASSWORD);
	auto *pbkeylen = obs_properties_add_list(props, "pbkeylen", "PB key length", OBS_COMBO_TYPE_LIST, OBS_COMBO_FORMAT_INT);
	obs_property_list_add_int(pbkeylen, "16", 16);
	obs_property_list_add_int(pbkeylen, "24", 24);
	obs_property_list_add_int(pbkeylen, "32", 32);
	auto *scheduler = obs_properties_add_list(props, "scheduler", "Scheduler", OBS_COMBO_TYPE_LIST, OBS_COMBO_FORMAT_STRING);
	obs_property_list_add_string(scheduler, "Enhanced", "enhanced");
	obs_property_list_add_string(scheduler, "Classic", "classic");
	return props;
}

static uint64_t srtla_output_total_bytes(void *opaque)
{
	return static_cast<SrtlaOutput *>(opaque)->total_bytes.load();
}

static int srtla_output_dropped_frames(void *opaque)
{
	return static_cast<SrtlaOutput *>(opaque)->dropped_frames.load();
}

struct obs_output_info srtla_output_info = [] {
	struct obs_output_info info{};
	info.id = "obs_srtla_output";
	info.flags = OBS_OUTPUT_AV | OBS_OUTPUT_ENCODED;
	info.get_name = srtla_output_name;
	info.create = srtla_output_create;
	info.destroy = srtla_output_destroy;
	info.start = srtla_output_start;
	info.stop = srtla_output_stop;
	info.encoded_packet = srtla_output_encoded_packet;
	info.update = srtla_output_update;
	info.get_defaults = srtla_output_defaults;
	info.get_properties = srtla_output_properties;
	info.get_total_bytes = srtla_output_total_bytes;
	info.get_dropped_frames = srtla_output_dropped_frames;
	info.encoded_video_codecs = "h264;hevc";
	info.encoded_audio_codecs = "aac;opus";
	return info;
}();

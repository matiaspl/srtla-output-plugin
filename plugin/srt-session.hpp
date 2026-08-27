#pragma once

#include "../engine/include/srtla_engine.h"

#include <atomic>
#include <cstddef>
#include <cstdint>
#include <condition_variable>
#include <functional>
#include <memory>
#include <mutex>
#include <string>
#include <thread>

enum class SrtSessionStatus : std::uint32_t {
	Idle = 0,
	Connecting = 1,
	Connected = 2,
	Reconnecting = 3,
	Fatal = 4,
	Stopped = 5,
};

class SrtSession final {
public:
	using Status = SrtSessionStatus;
	using StatusCallback = std::function<void(Status, const std::string &error)>;

	struct Impl;
	SrtSession(SrtlaEngineHandle *engine, std::string host, std::uint16_t port,
	           std::string stream_id, std::string passphrase, int latency_ms, int pbkeylen,
	           StatusCallback status_callback = {});
	~SrtSession();

	bool start();
	void stop();
	bool wait_connected(std::uint32_t timeout_ms);
	bool connected() const { return connected_.load(); }
	Status status() const;
	std::string last_error() const;
	bool send_ts(const std::uint8_t *data, std::size_t size);
	bool sample_stats(std::uint64_t sampled_at_ms, SrtlaSrtStatsV1 &stats);

private:
	std::unique_ptr<Impl> impl_;
	std::atomic_bool connected_{false};
};

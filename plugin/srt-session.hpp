#pragma once

#include "../engine/include/srtla_engine.h"

#include <atomic>
#include <cstddef>
#include <cstdint>
#include <memory>
#include <mutex>
#include <string>
#include <thread>

class SrtSession final {
public:
	struct Impl;
	SrtSession(SrtlaEngineHandle *engine, std::string host, std::uint16_t port,
	           std::string stream_id, std::string passphrase, int latency_ms, int pbkeylen);
	~SrtSession();

	bool start();
	void stop();
	bool connected() const { return connected_.load(); }
	bool send_ts(const std::uint8_t *data, std::size_t size);

private:
	std::unique_ptr<Impl> impl_;
	std::atomic_bool connected_{false};
};

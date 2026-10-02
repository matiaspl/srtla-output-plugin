#include "srt.h"

#include <cassert>
#include <atomic>
#include <chrono>
#include <condition_variable>
#include <cstddef>
#include <cstdint>
#include <mutex>
#include <thread>

namespace {
struct TransportState {
	std::atomic<int> wake_count{0};
	std::atomic<int> close_count{0};
	std::mutex mutex;
	std::condition_variable changed;
	bool receive_entered = false;
	bool allow_receive_return = false;
	bool receive_finished = false;
};

int send_datagram(void *, const std::uint8_t *, std::size_t, const std::uint8_t *, std::size_t,
                 const sockaddr_storage *)
{
	return 0;
}

int receive_datagram(void *opaque, std::uint8_t *, std::size_t, std::size_t *received,
                    sockaddr_storage *, int)
{
	auto *state = static_cast<TransportState *>(opaque);
	std::unique_lock<std::mutex> lock(state->mutex);
	state->receive_entered = true;
	state->changed.notify_all();
	state->changed.wait(lock, [state] { return state->allow_receive_return; });
	state->receive_finished = true;
	state->changed.notify_all();
	if (received)
		*received = 0;
	return 0;
}

void wake(void *opaque)
{
	auto *state = static_cast<TransportState *>(opaque);
	std::lock_guard<std::mutex> lock(state->mutex);
	++state->wake_count;
	state->changed.notify_all();
}
void close_transport(void *opaque)
{
	auto *state = static_cast<TransportState *>(opaque);
	std::lock_guard<std::mutex> lock(state->mutex);
	++state->close_count;
	state->changed.notify_all();
}
} // namespace

int main()
{
	TransportState state;
	SRT_TRANSPORT_V1 transport{};
	transport.version = SRT_TRANSPORT_V1_VERSION;
	transport.size = sizeof(transport);
	transport.opaque = &state;
	transport.send_datagram = send_datagram;
	transport.receive_datagram = receive_datagram;
	transport.wake = wake;
	transport.close = close_transport;

	assert(srt_startup() == 0);
	const SRTSOCKET socket = srt_create_socket();
	assert(socket != SRT_INVALID_SOCK);
	assert(srt_set_external_transport(socket, &transport) == 0);
	sockaddr_storage logical{};
	logical.ss_family = AF_INET;
	auto *logical_v4 = reinterpret_cast<sockaddr_in *>(&logical);
	logical_v4->sin_family = AF_INET;
	logical_v4->sin_addr.s_addr = htonl(INADDR_ANY);
	logical_v4->sin_port = 0;
	assert(srt_bind(socket, reinterpret_cast<const sockaddr *>(&logical), sizeof(sockaddr_in)) == 0);
	{
		std::unique_lock<std::mutex> lock(state.mutex);
		assert(state.changed.wait_for(lock, std::chrono::seconds(2), [&state] { return state.receive_entered; }));
	}
	std::atomic<bool> close_returned{false};
	std::thread closing([&state, socket, &close_returned] {
		assert(srt_close(socket) == 0);
		std::lock_guard<std::mutex> lock(state.mutex);
		close_returned.store(true);
		state.changed.notify_all();
	});
	bool close_returned_while_callback_active = false;
	{
		std::unique_lock<std::mutex> lock(state.mutex);
		assert(state.changed.wait_for(lock, std::chrono::seconds(2), [&state] {
			return state.wake_count.load() > 0;
		}));
		close_returned_while_callback_active = state.changed.wait_for(
			lock, std::chrono::milliseconds(30), [&close_returned] { return close_returned.load(); });
		state.allow_receive_return = true;
		state.changed.notify_all();
	}
	closing.join();
	assert(!close_returned_while_callback_active);
	assert(close_returned.load());
	assert(state.wake_count.load() == 1);
	assert(state.close_count.load() == 1);
	{
		std::lock_guard<std::mutex> lock(state.mutex);
		assert(state.receive_finished);
	}

	// A streaming session creates another SRT socket after a reconnect. Exercise
	// repeated bind/close cycles while previous sockets are still eligible for
	// asynchronous GC, as happens when an uplink disappears during a stream.
	for (int cycle = 0; cycle < 24; ++cycle) {
		{
			std::lock_guard<std::mutex> lock(state.mutex);
			state.receive_entered = false;
			state.receive_finished = false;
		}
		const SRTSOCKET reconnect_socket = srt_create_socket();
		assert(reconnect_socket != SRT_INVALID_SOCK);
		assert(srt_set_external_transport(reconnect_socket, &transport) == 0);
		assert(srt_bind(reconnect_socket, reinterpret_cast<const sockaddr *>(&logical), sizeof(sockaddr_in)) == 0);
		{
			std::unique_lock<std::mutex> lock(state.mutex);
			assert(state.changed.wait_for(lock, std::chrono::seconds(2), [&state] {
				return state.receive_entered;
			}));
		}
		assert(srt_close(reconnect_socket) == 0);
		assert(state.wake_count.load() == cycle + 2);
		assert(state.close_count.load() == cycle + 2);
		std::lock_guard<std::mutex> lock(state.mutex);
		assert(state.receive_finished);
	}
	assert(srt_cleanup() == 0);
}

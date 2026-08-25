#include "srt.h"

#include <cassert>
#include <atomic>
#include <cstddef>
#include <cstdint>

namespace {
struct TransportState {
	std::atomic<int> wake_count{0};
	std::atomic<int> close_count{0};
};

int send_datagram(void *, const std::uint8_t *, std::size_t, const std::uint8_t *, std::size_t,
                 const sockaddr_storage *)
{
	return 0;
}

int receive_datagram(void *opaque, std::uint8_t *, std::size_t, std::size_t *received,
                    sockaddr_storage *, int)
{
	if (static_cast<TransportState *>(opaque)->wake_count.load() > 0)
		return -1;
	if (received)
		*received = 0;
	return 0;
}

void wake(void *opaque) { ++static_cast<TransportState *>(opaque)->wake_count; }
void close_transport(void *opaque) { ++static_cast<TransportState *>(opaque)->close_count; }
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
	assert(srt_close(socket) == 0);
	assert(state.wake_count.load() == 1);
	assert(state.close_count.load() == 1);
	assert(srt_cleanup() == 0);
}

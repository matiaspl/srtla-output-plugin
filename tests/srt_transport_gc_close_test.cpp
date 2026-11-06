// Regression test: an embedded-transport socket that breaks and is moved to
// the closed list by libsrt's garbage collector before the application calls
// srt_close() must not keep invoking transport callbacks afterwards.
//
// Before the fix, srt_close() could not find such a socket and returned
// without closing its callback channel, while the receive worker kept calling
// receive_datagram() with the application's opaque pointer until the GC
// removed the socket at least one GC period later.  The OBS plugin frees that
// opaque context (and the SRTLA engine it references) as soon as the output
// stops, which produced occasional use-after-free crashes.

#include "srt.h"

#ifdef _WIN32
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <winsock2.h>
#include <ws2tcpip.h>
using native_socket = SOCKET;
static void close_native(native_socket s) { closesocket(s); }
#else
#include <arpa/inet.h>
#include <netinet/in.h>
#include <poll.h>
#include <sys/socket.h>
#include <unistd.h>
using native_socket = int;
static void close_native(native_socket s) { ::close(s); }
#endif

#include <atomic>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <thread>
#include <vector>

#define CHECK(cond)                                                                  \
	do {                                                                             \
		if (!(cond)) {                                                               \
			std::fprintf(stderr, "%s:%d: check failed: %s\n", __FILE__, __LINE__, #cond); \
			std::exit(1);                                                            \
		}                                                                            \
	} while (0)

namespace {
struct Bridge {
	native_socket udp;
	sockaddr_in peer{};
	std::atomic<bool> released{false};
	std::atomic<int> calls_after_release{0};
};

void note_call(Bridge *bridge)
{
	if (bridge->released.load())
		bridge->calls_after_release.fetch_add(1);
}

int send_datagram(void *opaque, const std::uint8_t *header, std::size_t header_len,
                 const std::uint8_t *payload, std::size_t payload_len, const sockaddr_storage *)
{
	auto *bridge = static_cast<Bridge *>(opaque);
	note_call(bridge);
	std::vector<std::uint8_t> datagram(header, header + header_len);
	datagram.insert(datagram.end(), payload, payload + payload_len);
	const auto sent = ::sendto(bridge->udp, reinterpret_cast<const char *>(datagram.data()),
	                           static_cast<int>(datagram.size()), 0,
	                           reinterpret_cast<const sockaddr *>(&bridge->peer), sizeof(bridge->peer));
	return sent == static_cast<decltype(sent)>(datagram.size()) ? 0 : -1;
}

int receive_datagram(void *opaque, std::uint8_t *buffer, std::size_t capacity, std::size_t *received,
                    sockaddr_storage *source, int timeout_ms)
{
	auto *bridge = static_cast<Bridge *>(opaque);
	note_call(bridge);
	*received = 0;
#ifdef _WIN32
	WSAPOLLFD pfd{bridge->udp, POLLRDNORM, 0};
	if (WSAPoll(&pfd, 1, timeout_ms) <= 0)
		return 0;
#else
	pollfd pfd{bridge->udp, POLLIN, 0};
	if (::poll(&pfd, 1, timeout_ms) <= 0)
		return 0;
#endif
	const auto n = ::recv(bridge->udp, reinterpret_cast<char *>(buffer), static_cast<int>(capacity), 0);
	if (n <= 0)
		return 0;
	*received = static_cast<std::size_t>(n);
	std::memset(source, 0, sizeof(*source));
	std::memcpy(source, &bridge->peer, sizeof(bridge->peer));
	return 1;
}
} // namespace

int main()
{
#ifdef _WIN32
	WSADATA wsa;
	CHECK(WSAStartup(MAKEWORD(2, 2), &wsa) == 0);
#endif
	CHECK(srt_startup() >= 0);

	// Ordinary SRT listener on loopback.
	const SRTSOCKET listener = srt_create_socket();
	CHECK(listener != SRT_INVALID_SOCK);
	sockaddr_in listen_addr{};
	listen_addr.sin_family = AF_INET;
	listen_addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
	listen_addr.sin_port = 0;
	CHECK(srt_bind(listener, reinterpret_cast<sockaddr *>(&listen_addr), sizeof(listen_addr)) == 0);
	int addr_len = sizeof(listen_addr);
	CHECK(srt_getsockname(listener, reinterpret_cast<sockaddr *>(&listen_addr), &addr_len) == 0);
	CHECK(srt_listen(listener, 1) == 0);

	// Embedded-transport caller, bridged to the listener over a kernel UDP socket.
	auto *bridge = new Bridge;
	bridge->udp = ::socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
	sockaddr_in local{};
	local.sin_family = AF_INET;
	local.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
	CHECK(::bind(bridge->udp, reinterpret_cast<sockaddr *>(&local), sizeof(local)) == 0);
	bridge->peer = listen_addr;

	SRT_TRANSPORT_V1 transport{};
	transport.version = SRT_TRANSPORT_V1_VERSION;
	transport.size = sizeof(transport);
	transport.opaque = bridge;
	transport.send_datagram = send_datagram;
	transport.receive_datagram = receive_datagram;

	const SRTSOCKET caller = srt_create_socket();
	CHECK(caller != SRT_INVALID_SOCK);
	CHECK(srt_set_external_transport(caller, &transport) == 0);
	// Detect the broken peer quickly instead of after the default five seconds.
	int peer_idle_ms = 1000;
	CHECK(srt_setsockopt(caller, 0, SRTO_PEERIDLETIMEO, &peer_idle_ms, sizeof(peer_idle_ms)) == 0);
	sockaddr_in any{};
	any.sin_family = AF_INET;
	CHECK(srt_bind(caller, reinterpret_cast<sockaddr *>(&any), sizeof(any)) == 0);

	SRTSOCKET accepted = SRT_INVALID_SOCK;
	std::thread acceptor([&] {
		sockaddr_storage peer{};
		int peer_len = sizeof(peer);
		accepted = srt_accept(listener, reinterpret_cast<sockaddr *>(&peer), &peer_len);
	});
	CHECK(srt_connect(caller, reinterpret_cast<sockaddr *>(&listen_addr), sizeof(listen_addr)) == 0);
	acceptor.join();
	CHECK(accepted != SRT_INVALID_SOCK);
	CHECK(srt_getsockstate(caller) == SRTS_CONNECTED);

	// Break the connection from the remote side and let the GC move the caller
	// to its closed list before the application reacts.
	CHECK(srt_close(accepted) == 0);
	bool moved_by_gc = false;
	for (int i = 0; i < 200 && !moved_by_gc; ++i) {
		const auto state = srt_getsockstate(caller);
		moved_by_gc = state == SRTS_CLOSED || state == SRTS_NONEXIST;
		if (!moved_by_gc)
			std::this_thread::sleep_for(std::chrono::milliseconds(50));
	}
	CHECK(moved_by_gc);

	// The application now closes its handle (this may report an error because
	// the socket is no longer active) and releases the transport context.
	(void)srt_close(caller);
	bridge->released.store(true);

	// No callback may reach the released context, even though the multiplexer
	// stays alive until the GC removes the socket.
	std::this_thread::sleep_for(std::chrono::milliseconds(300));
	const int late_calls = bridge->calls_after_release.load();
	if (late_calls != 0)
		std::fprintf(stderr, "transport called %d times after srt_close()\n", late_calls);
	CHECK(late_calls == 0);

	CHECK(srt_close(listener) == 0);
	CHECK(srt_cleanup() == 0);
	close_native(bridge->udp);
	delete bridge;
#ifdef _WIN32
	WSACleanup();
#endif
	return 0;
}

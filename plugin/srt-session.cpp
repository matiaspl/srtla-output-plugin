#include "srt-session.hpp"

#include <srt.h>

#ifdef _WIN32
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <winsock2.h>
#include <ws2tcpip.h>
#else
#include <arpa/inet.h>
#include <netdb.h>
#include <sys/socket.h>
#endif

#include <chrono>
#include <algorithm>
#include <cstring>
#include <utility>
#include <vector>

struct SrtSession::Impl {
	Impl(SrtlaEngineHandle *engine_, std::string host_, std::uint16_t port_, std::string stream_id_,
	     std::string passphrase_, int latency_ms_, int pbkeylen_)
		: engine(engine_), host(std::move(host_)), port(port_), stream_id(std::move(stream_id_)),
		  passphrase(std::move(passphrase_)), latency_ms(latency_ms_), pbkeylen(pbkeylen_) {}

	SrtlaEngineHandle *engine;
	std::string host;
	std::uint16_t port;
	std::string stream_id;
	std::string passphrase;
	int latency_ms;
	int pbkeylen;
	std::mutex socket_mutex;
	std::mutex remote_mutex;
	SRTSOCKET socket = SRT_INVALID_SOCK;
	std::atomic_bool stop{false};
	std::atomic_bool transport_closed{false};
	std::atomic_bool wake_requested{false};
	std::thread connector;
	SRT_TRANSPORT_V1 transport{};
	sockaddr_storage remote{};
	socklen_t remote_len = 0;
};

namespace {
std::once_flag startup_once;

void ensure_srt_startup()
{
	std::call_once(startup_once, [] { (void)srt_startup(); });
}

int transport_send(void *opaque, const std::uint8_t *header, std::size_t header_len,
	                 const std::uint8_t *payload, std::size_t payload_len,
	                 const sockaddr_storage *)
{
	auto *session = static_cast<SrtSession::Impl *>(opaque);
	if (!session || session->stop.load())
		return -1;
	if (session->wake_requested.exchange(false))
		return 0;
	try {
		std::vector<std::uint8_t> datagram;
		datagram.reserve(header_len + payload_len);
		if (header_len)
			datagram.insert(datagram.end(), header, header + header_len);
		if (payload_len)
			datagram.insert(datagram.end(), payload, payload + payload_len);
		const int result = srtla_engine_submit_srt_datagram(session->engine, datagram.data(), datagram.size());
		return result == 0 ? 0 : -1;
	} catch (...) {
		return -1;
	}
}

int transport_receive(void *opaque, std::uint8_t *buffer, std::size_t capacity, std::size_t *received,
	                  sockaddr_storage *source, int timeout_ms)
{
	auto *session = static_cast<SrtSession::Impl *>(opaque);
	if (received)
		*received = 0;
	if (!session || session->stop.load())
		return -1;
	if (session->wake_requested.exchange(false))
		return -1;
	const int result = srtla_engine_receive_srt_datagram(session->engine, buffer, capacity,
		static_cast<std::uint32_t>(timeout_ms < 0 ? 0 : timeout_ms));
	if (result <= 0)
		return result;
	if (received)
		*received = static_cast<std::size_t>(result);
	if (source) {
		std::memset(source, 0, sizeof(*source));
		std::lock_guard<std::mutex> lock(session->remote_mutex);
		std::memcpy(source, &session->remote, session->remote_len);
	}
	return 1;
}

void transport_wake(void *opaque)
{
	if (auto *session = static_cast<SrtSession::Impl *>(opaque))
		session->wake_requested.store(true);
}

void transport_close(void *opaque)
{
	if (auto *session = static_cast<SrtSession::Impl *>(opaque)) {
		session->transport_closed.store(true);
		session->wake_requested.store(true);
	}
}

bool resolve_remote(const std::string &host, std::uint16_t port, sockaddr_storage &remote, socklen_t &length)
{
	addrinfo hints{};
	hints.ai_socktype = SOCK_DGRAM;
	hints.ai_protocol = IPPROTO_UDP;
	addrinfo *result = nullptr;
	const auto service = std::to_string(port);
	if (getaddrinfo(host.c_str(), service.c_str(), &hints, &result) != 0 || !result)
		return false;
	std::memcpy(&remote, result->ai_addr, static_cast<std::size_t>(result->ai_addrlen));
	length = static_cast<socklen_t>(result->ai_addrlen);
	freeaddrinfo(result);
	return true;
}
} // namespace

SrtSession::SrtSession(SrtlaEngineHandle *engine, std::string host, std::uint16_t port,
	                   std::string stream_id, std::string passphrase, int latency_ms, int pbkeylen)
	: impl_(std::make_unique<Impl>(engine, std::move(host), port, std::move(stream_id),
	                               std::move(passphrase), latency_ms, pbkeylen))
{
	impl_->transport.version = SRT_TRANSPORT_V1_VERSION;
	impl_->transport.size = sizeof(SRT_TRANSPORT_V1);
	impl_->transport.opaque = impl_.get();
	impl_->transport.send_datagram = transport_send;
	impl_->transport.receive_datagram = transport_receive;
	impl_->transport.wake = transport_wake;
	impl_->transport.close = transport_close;
}

SrtSession::~SrtSession() { stop(); }

bool SrtSession::start()
{
	if (!impl_ || impl_->connector.joinable())
		return false;
	ensure_srt_startup();
	impl_->stop.store(false);
	impl_->wake_requested.store(false);
	impl_->transport_closed.store(false);
	try {
		impl_->connector = std::thread([this] {
		for (unsigned backoff = 1; !impl_->stop.load(); backoff = backoff == 1 ? 2U : 5U) {
			sockaddr_storage remote{};
			socklen_t remote_len = 0;
			if (!resolve_remote(impl_->host, impl_->port, remote, remote_len)) {
				for (unsigned wait = 0; wait < backoff * 10 && !impl_->stop.load(); ++wait)
					std::this_thread::sleep_for(std::chrono::milliseconds(100));
				continue;
			}
			{
				std::lock_guard<std::mutex> lock(impl_->remote_mutex);
				impl_->remote = remote;
				impl_->remote_len = remote_len;
			}
			const SRTSOCKET socket = srt_create_socket();
			if (socket == SRT_INVALID_SOCK)
				return;
			{
				std::lock_guard<std::mutex> lock(impl_->socket_mutex);
				impl_->socket = socket;
			}
			if (impl_->stop.load()) {
				std::lock_guard<std::mutex> lock(impl_->socket_mutex);
				srt_close(socket);
				impl_->socket = SRT_INVALID_SOCK;
				return;
			}
			int latency = impl_->latency_ms;
			(void)srt_setsockopt(socket, 0, SRTO_LATENCY, &latency, sizeof(latency));
			int connect_timeout = 1000;
			(void)srt_setsockopt(socket, 0, SRTO_CONNTIMEO, &connect_timeout, sizeof(connect_timeout));
			bool send_synchronous = false;
			(void)srt_setsockopt(socket, 0, SRTO_SNDSYN, &send_synchronous, sizeof(send_synchronous));
			if (!impl_->passphrase.empty())
				(void)srt_setsockopt(socket, 0, SRTO_PASSPHRASE, impl_->passphrase.c_str(), static_cast<int>(impl_->passphrase.size()));
			if (impl_->pbkeylen == 16 || impl_->pbkeylen == 24 || impl_->pbkeylen == 32)
				(void)srt_setsockopt(socket, 0, SRTO_PBKEYLEN, &impl_->pbkeylen, sizeof(impl_->pbkeylen));
			if (!impl_->stream_id.empty())
				(void)srt_setsockopt(socket, 0, SRTO_STREAMID, impl_->stream_id.c_str(), static_cast<int>(impl_->stream_id.size()));
			if (impl_->remote.ss_family == AF_INET6) {
				// libsrt requires an explicit IPv6-only policy when the logical
				// bind address is the wildcard ::.  No kernel socket is created;
				// this only selects the address family used in the handshake.
				int ipv6_only = 1;
				(void)srt_setsockopt(socket, 0, SRTO_IPV6ONLY, &ipv6_only, sizeof(ipv6_only));
			}
			if (srt_set_external_transport(socket, &impl_->transport) != 0) {
				std::lock_guard<std::mutex> lock(impl_->socket_mutex);
				srt_close(socket);
				impl_->socket = SRT_INVALID_SOCK;
				return;
			}
			sockaddr_storage local{};
			local.ss_family = impl_->remote.ss_family;
			if (impl_->remote.ss_family == AF_INET) {
				auto *addr = reinterpret_cast<sockaddr_in *>(&local);
				addr->sin_family = AF_INET;
				addr->sin_port = 0;
			} else {
				auto *addr = reinterpret_cast<sockaddr_in6 *>(&local);
				addr->sin6_family = AF_INET6;
				addr->sin6_port = 0;
			}
			if (srt_bind(socket, reinterpret_cast<sockaddr *>(&local), impl_->remote.ss_family == AF_INET ? sizeof(sockaddr_in) : sizeof(sockaddr_in6)) != 0 ||
				srt_connect(socket, reinterpret_cast<sockaddr *>(&impl_->remote), impl_->remote_len) != 0) {
				std::lock_guard<std::mutex> lock(impl_->socket_mutex);
				srt_close(socket);
				impl_->socket = SRT_INVALID_SOCK;
				for (unsigned wait = 0; wait < backoff * 10 && !impl_->stop.load(); ++wait)
					std::this_thread::sleep_for(std::chrono::milliseconds(100));
				continue;
			}
			connected_.store(true);
			while (!impl_->stop.load()) {
				const auto status = srt_getsockstate(socket);
				if (status == SRTS_BROKEN || status == SRTS_CLOSED || status == SRTS_NONEXIST)
					break;
				std::this_thread::sleep_for(std::chrono::milliseconds(100));
			}
			connected_.store(false);
			{
				std::lock_guard<std::mutex> lock(impl_->socket_mutex);
				if (impl_->socket == socket) {
					srt_close(socket);
					impl_->socket = SRT_INVALID_SOCK;
				}
			}
			if (!impl_->stop.load())
				for (unsigned wait = 0; wait < backoff * 10 && !impl_->stop.load(); ++wait)
					std::this_thread::sleep_for(std::chrono::milliseconds(100));
		}
		});
	} catch (...) {
		impl_->stop.store(true);
		connected_.store(false);
		return false;
	}
	return true;
}

void SrtSession::stop()
{
	if (!impl_)
		return;
	impl_->stop.store(true);
	{
		std::lock_guard<std::mutex> lock(impl_->socket_mutex);
		if (impl_->socket != SRT_INVALID_SOCK) {
			srt_close(impl_->socket);
			impl_->socket = SRT_INVALID_SOCK;
		}
	}
	if (impl_->connector.joinable())
		impl_->connector.join();
	connected_.store(false);
}

bool SrtSession::send_ts(const std::uint8_t *data, std::size_t size)
{
	if (!data || size == 0 || !connected_.load() || !impl_)
		return false;
	std::lock_guard<std::mutex> lock(impl_->socket_mutex);
	if (impl_->socket == SRT_INVALID_SOCK)
		return false;
	return srt_sendmsg(impl_->socket, reinterpret_cast<const char *>(data), static_cast<int>(size), -1, 0) == static_cast<int>(size);
}

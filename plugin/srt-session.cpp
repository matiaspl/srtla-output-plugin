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
#include <cmath>
#include <cstring>
#include <limits>
#include <utility>
#include <vector>

struct SrtSession::Impl {
	Impl(SrtlaEngineHandle *engine_, std::string host_, std::uint16_t port_, std::string stream_id_,
	     std::string passphrase_, int latency_ms_, int pbkeylen_, StatusCallback status_callback_)
		: engine(engine_), host(std::move(host_)), port(port_), stream_id(std::move(stream_id_)),
		  passphrase(std::move(passphrase_)), latency_ms(latency_ms_),
		  negotiated_latency_ms(latency_ms_), pbkeylen(pbkeylen_),
		  status_callback(std::move(status_callback_)) {}

	SrtlaEngineHandle *engine;
	std::string host;
	std::uint16_t port;
	std::string stream_id;
	std::string passphrase;
	int latency_ms;
	std::atomic<int> negotiated_latency_ms;
	int pbkeylen;
	std::mutex socket_mutex;
	std::mutex remote_mutex;
	SRTSOCKET socket = SRT_INVALID_SOCK;
	std::atomic_bool stop{false};
	std::atomic_bool transport_closed{false};
	std::atomic_bool wake_requested{false};
	std::thread connector;
	StatusCallback status_callback;
	mutable std::mutex status_mutex;
	std::condition_variable status_changed;
	SrtSession::Status session_status = SrtSession::Status::Idle;
	std::string error;
	SRT_TRANSPORT_V1 transport{};
	sockaddr_storage remote{};
	socklen_t remote_len = 0;
};

namespace {
std::once_flag startup_once;

void set_status(SrtSession::Impl *session, SrtSession::Status status, std::string error = {})
{
	if (!session)
		return;
	std::string callback_error;
	{
		std::lock_guard<std::mutex> lock(session->status_mutex);
		session->session_status = status;
		session->error = std::move(error);
		callback_error = session->error;
	}
	session->status_changed.notify_all();
	if (session->status_callback)
		session->status_callback(status, callback_error);
	const auto state = static_cast<SrtlaSessionState>(status);
	(void)srtla_engine_set_session_state(session->engine, state,
	                                     callback_error.empty() ? nullptr : callback_error.c_str());
}

std::string srt_error(const char *operation)
{
	const char *detail = srt_getlasterror_str();
	std::string message = operation ? operation : "SRT operation failed";
	if (detail && *detail) {
		message += ": ";
		message += detail;
	}
	return message;
}

bool valid_utf8(const std::string &value)
{
	for (std::size_t i = 0; i < value.size();) {
		const auto byte = static_cast<unsigned char>(value[i]);
		std::size_t width = 0;
		std::uint32_t codepoint = 0;
		if (byte <= 0x7f) {
			width = 1;
			codepoint = byte;
		} else if (byte >= 0xc2 && byte <= 0xdf) {
			width = 2;
			codepoint = byte & 0x1f;
		} else if (byte >= 0xe0 && byte <= 0xef) {
			width = 3;
			codepoint = byte & 0x0f;
		} else if (byte >= 0xf0 && byte <= 0xf4) {
			width = 4;
			codepoint = byte & 0x07;
		} else {
			return false;
		}
		if (i + width > value.size())
			return false;
		for (std::size_t j = 1; j < width; ++j) {
			const auto continuation = static_cast<unsigned char>(value[i + j]);
			if ((continuation & 0xc0) != 0x80)
				return false;
			codepoint = (codepoint << 6) | (continuation & 0x3f);
		}
		if ((width == 2 && codepoint < 0x80) || (width == 3 && codepoint < 0x800) ||
		    (width == 4 && (codepoint < 0x10000 || codepoint > 0x10ffff)) ||
		    (codepoint >= 0xd800 && codepoint <= 0xdfff))
			return false;
		i += width;
	}
	return true;
}

void ensure_srt_startup()
{
	std::call_once(startup_once, [] { (void)srt_startup(); });
}

std::uint64_t mbps_to_bps(double mbps)
{
	if (!std::isfinite(mbps) || mbps <= 0.0)
		return 0;
	const double bps = mbps * 1'000'000.0;
	if (bps >= static_cast<double>(std::numeric_limits<std::uint64_t>::max()))
		return std::numeric_limits<std::uint64_t>::max();
	return static_cast<std::uint64_t>(bps);
}

int transport_send(void *opaque, const std::uint8_t *header, std::size_t header_len,
	                 const std::uint8_t *payload, std::size_t payload_len,
	                 const sockaddr_storage *)
{
	auto *session = static_cast<SrtSession::Impl *>(opaque);
	if (!session || session->stop.load())
		return -1;
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
                       std::string stream_id, std::string passphrase, int latency_ms, int pbkeylen,
                       StatusCallback status_callback)
	: impl_(std::make_unique<Impl>(engine, std::move(host), port, std::move(stream_id),
                               std::move(passphrase), latency_ms, pbkeylen,
                               std::move(status_callback)))
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
	if ((!impl_->passphrase.empty() && (impl_->passphrase.size() < 10 || impl_->passphrase.size() > 79 ||
	                                    !valid_utf8(impl_->passphrase))) ||
	    (impl_->pbkeylen != 16 && impl_->pbkeylen != 24 && impl_->pbkeylen != 32)) {
		set_status(impl_.get(), Status::Fatal, "Invalid SRT encryption settings");
		return false;
	}
	ensure_srt_startup();
	impl_->stop.store(false);
	impl_->wake_requested.store(false);
	impl_->transport_closed.store(false);
	set_status(impl_.get(), Status::Connecting);
	try {
		impl_->connector = std::thread([this] {
		bool ever_connected = false;
		for (unsigned backoff = 1; !impl_->stop.load(); backoff = backoff == 1 ? 2U : 5U) {
			sockaddr_storage remote{};
			socklen_t remote_len = 0;
			if (!resolve_remote(impl_->host, impl_->port, remote, remote_len)) {
				if (ever_connected)
					set_status(impl_.get(), Status::Reconnecting, "Resolving SRT receiver");
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
			if (socket == SRT_INVALID_SOCK) {
				set_status(impl_.get(), Status::Fatal, srt_error("srt_create_socket"));
				return;
			}
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
			auto set_option = [&](SRT_SOCKOPT option, const void *value, int length, const char *name) {
				if (srt_setsockopt(socket, 0, option, value, length) == 0)
					return true;
				set_status(impl_.get(), Status::Fatal, srt_error(name));
				std::lock_guard<std::mutex> lock(impl_->socket_mutex);
				if (impl_->socket == socket)
					impl_->socket = SRT_INVALID_SOCK;
				srt_close(socket);
				return false;
			};
			int latency = impl_->latency_ms;
			if (!set_option(SRTO_LATENCY, &latency, sizeof(latency), "SRTO_LATENCY"))
				return;
			int connect_timeout = 1000;
			if (!set_option(SRTO_CONNTIMEO, &connect_timeout, sizeof(connect_timeout), "SRTO_CONNTIMEO"))
				return;
			bool send_synchronous = false;
			if (!set_option(SRTO_SNDSYN, &send_synchronous, sizeof(send_synchronous), "SRTO_SNDSYN"))
				return;
			if (!impl_->passphrase.empty() &&
			    !set_option(SRTO_PASSPHRASE, impl_->passphrase.c_str(), static_cast<int>(impl_->passphrase.size()), "SRTO_PASSPHRASE"))
				return;
			if (!set_option(SRTO_PBKEYLEN, &impl_->pbkeylen, sizeof(impl_->pbkeylen), "SRTO_PBKEYLEN"))
				return;
			if (!impl_->stream_id.empty() &&
			    !set_option(SRTO_STREAMID, impl_->stream_id.c_str(), static_cast<int>(impl_->stream_id.size()), "SRTO_STREAMID"))
				return;
			if (remote.ss_family == AF_INET6) {
				// libsrt requires an explicit IPv6-only policy when the logical
				// bind address is the wildcard ::.  No kernel socket is created;
				// this only selects the address family used in the handshake.
				int ipv6_only = 1;
				if (!set_option(SRTO_IPV6ONLY, &ipv6_only, sizeof(ipv6_only), "SRTO_IPV6ONLY"))
					return;
			}
			if (srt_set_external_transport(socket, &impl_->transport) != 0) {
				set_status(impl_.get(), Status::Fatal, srt_error("srt_set_external_transport"));
				std::lock_guard<std::mutex> lock(impl_->socket_mutex);
				srt_close(socket);
				impl_->socket = SRT_INVALID_SOCK;
				return;
			}
			sockaddr_storage local{};
			local.ss_family = remote.ss_family;
			if (remote.ss_family == AF_INET) {
				auto *addr = reinterpret_cast<sockaddr_in *>(&local);
				addr->sin_family = AF_INET;
				addr->sin_port = 0;
			} else {
				auto *addr = reinterpret_cast<sockaddr_in6 *>(&local);
				addr->sin6_family = AF_INET6;
				addr->sin6_port = 0;
			}
			if (srt_bind(socket, reinterpret_cast<sockaddr *>(&local), remote.ss_family == AF_INET ? sizeof(sockaddr_in) : sizeof(sockaddr_in6)) != 0) {
				const auto error = srt_error("srt_bind");
				set_status(impl_.get(), ever_connected ? Status::Reconnecting : Status::Connecting, error);
				std::lock_guard<std::mutex> lock(impl_->socket_mutex);
				srt_close(socket);
				impl_->socket = SRT_INVALID_SOCK;
				for (unsigned wait = 0; wait < backoff * 10 && !impl_->stop.load(); ++wait)
					std::this_thread::sleep_for(std::chrono::milliseconds(100));
				continue;
			}
			if (srt_connect(socket, reinterpret_cast<sockaddr *>(&remote), remote_len) != 0) {
				const auto error = srt_error("srt_connect");
				if (!ever_connected)
					set_status(impl_.get(), Status::Connecting, error);
				else
					set_status(impl_.get(), Status::Reconnecting, error);
				std::lock_guard<std::mutex> lock(impl_->socket_mutex);
				srt_close(socket);
				impl_->socket = SRT_INVALID_SOCK;
				for (unsigned wait = 0; wait < backoff * 10 && !impl_->stop.load(); ++wait)
					std::this_thread::sleep_for(std::chrono::milliseconds(100));
				continue;
			}
			// srt_connect can return before the handshake completes.  Do not
			// expose Connected (or let OBS begin capture) until libsrt reports the
			// established state.  A failed initial handshake remains retryable so
			// the five-second wait in the output can produce a bounded start error,
			// while an established stream reconnects indefinitely.
			bool handshake_connected = false;
			for (unsigned wait = 0; wait < 50 && !impl_->stop.load(); ++wait) {
				const auto socket_state = srt_getsockstate(socket);
				if (socket_state == SRTS_CONNECTED) {
					handshake_connected = true;
					break;
				}
				if (socket_state == SRTS_BROKEN || socket_state == SRTS_CLOSING ||
				    socket_state == SRTS_CLOSED || socket_state == SRTS_NONEXIST)
					break;
				std::this_thread::sleep_for(std::chrono::milliseconds(100));
			}
			if (!handshake_connected) {
				const auto error = srt_error("SRT connection handshake");
				set_status(impl_.get(), ever_connected ? Status::Reconnecting : Status::Connecting,
				           error.empty() ? "SRT connection timed out" : error);
				std::lock_guard<std::mutex> lock(impl_->socket_mutex);
				srt_close(socket);
				impl_->socket = SRT_INVALID_SOCK;
				continue;
			}
			int negotiated_latency = impl_->latency_ms;
			int negotiated_latency_size = sizeof(negotiated_latency);
			if (srt_getsockflag(socket, SRTO_PEERLATENCY, &negotiated_latency,
			                    &negotiated_latency_size) == 0 && negotiated_latency > 0)
				impl_->negotiated_latency_ms.store(negotiated_latency);
			else
				impl_->negotiated_latency_ms.store(impl_->latency_ms);
			backoff = 1;
			connected_.store(true);
			ever_connected = true;
			set_status(impl_.get(), Status::Connected);
			while (!impl_->stop.load()) {
				const auto status = srt_getsockstate(socket);
				if (status == SRTS_BROKEN || status == SRTS_CLOSED || status == SRTS_NONEXIST)
					break;
				std::this_thread::sleep_for(std::chrono::milliseconds(100));
			}
			connected_.store(false);
			if (!impl_->stop.load())
				set_status(impl_.get(), Status::Reconnecting, "SRT session disconnected");
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
		set_status(impl_.get(), Status::Fatal, "Failed to start SRT connector thread");
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
	set_status(impl_.get(), Status::Stopped);
}

bool SrtSession::wait_connected(std::uint32_t timeout_ms)
{
	if (!impl_)
		return false;
	std::unique_lock<std::mutex> lock(impl_->status_mutex);
	const auto ready = impl_->status_changed.wait_for(lock, std::chrono::milliseconds(timeout_ms), [this] {
		return impl_->session_status == Status::Connected || impl_->session_status == Status::Fatal ||
			impl_->session_status == Status::Stopped;
	});
	return ready && impl_->session_status == Status::Connected;
}

SrtSession::Status SrtSession::status() const
{
	if (!impl_)
		return Status::Stopped;
	std::lock_guard<std::mutex> lock(impl_->status_mutex);
	return impl_->session_status;
}

std::string SrtSession::last_error() const
{
	if (!impl_)
		return "SRT session is unavailable";
	std::lock_guard<std::mutex> lock(impl_->status_mutex);
	return impl_->error;
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

bool SrtSession::sample_stats(std::uint64_t sampled_at_ms, SrtlaSrtStats &stats)
{
	stats = {};
	stats.struct_size = static_cast<std::uint32_t>(sizeof(stats));
	if (!connected_.load() || !impl_)
		return false;

	std::lock_guard<std::mutex> lock(impl_->socket_mutex);
	if (impl_->socket == SRT_INVALID_SOCK)
		return false;

	SRT_TRACEBSTATS perf{};
	if (srt_bistats(impl_->socket, &perf, 1, 1) == SRT_ERROR)
		return false;
	int send_buffer_packets = 0;
	int send_buffer_packets_size = sizeof(send_buffer_packets);
	if (srt_getsockflag(impl_->socket, SRTO_SNDDATA, &send_buffer_packets,
	                    &send_buffer_packets_size) == SRT_ERROR || send_buffer_packets < 0)
		return false;

	stats.sampled_at_ms = sampled_at_ms;
	stats.bandwidth_bps = mbps_to_bps(perf.mbpsBandwidth);
	stats.send_rate_bps = mbps_to_bps(perf.mbpsSendRate);
	stats.sent_unique_bytes = perf.byteSentUnique;
	stats.retransmitted_bytes = perf.byteRetrans;
	stats.dropped_bytes = perf.byteSndDrop;
	stats.send_buffer_ms = static_cast<std::uint32_t>(std::max(0, perf.msSndBuf));
	stats.packets_in_flight = static_cast<std::uint32_t>(std::max(0, perf.pktFlightSize));
	stats.sender_loss_packets = static_cast<std::uint32_t>(std::max(0, perf.pktSndLoss));
	stats.rtt_ms = static_cast<std::uint32_t>(std::max(0.0, perf.msRTT));
	stats.send_buffer_packets = static_cast<std::uint32_t>(send_buffer_packets);
	stats.latency_ms = static_cast<std::uint32_t>(std::max(1, impl_->negotiated_latency_ms.load()));
	return true;
}

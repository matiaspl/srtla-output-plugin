#include "network-monitor.hpp"

#ifdef _WIN32
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#ifndef NOMINMAX
#define NOMINMAX
#endif
#include <winsock2.h>
#include <iphlpapi.h>
#include <ws2tcpip.h>
#include <windows.h>

#include <array>
#include <cstring>
#include <unordered_set>
#include <utility>

std::vector<NetworkAdapter> NetworkMonitor::enumerate() const
{
	ULONG size = 16 * 1024;
	std::vector<unsigned char> buffer(size);
	auto *addresses = reinterpret_cast<IP_ADAPTER_ADDRESSES *>(buffer.data());
	ULONG result = GetAdaptersAddresses(AF_UNSPEC, GAA_FLAG_INCLUDE_PREFIX, nullptr, addresses, &size);
	if (result == ERROR_BUFFER_OVERFLOW) {
		buffer.resize(size);
		addresses = reinterpret_cast<IP_ADAPTER_ADDRESSES *>(buffer.data());
		result = GetAdaptersAddresses(AF_UNSPEC, GAA_FLAG_INCLUDE_PREFIX, nullptr, addresses, &size);
	}
	if (result != NO_ERROR)
		return {};

	std::vector<NetworkAdapter> result_list;
	for (auto *adapter = addresses; adapter; adapter = adapter->Next) {
		if (adapter->IfType == IF_TYPE_SOFTWARE_LOOPBACK || adapter->IfType == IF_TYPE_TUNNEL)
			continue;
		char guid[64] = {};
		if (adapter->AdapterName)
			strncpy_s(guid, sizeof(guid), adapter->AdapterName, sizeof(guid) - 1);
		std::string label;
		if (adapter->FriendlyName) {
			char utf8[256] = {};
			WideCharToMultiByte(CP_UTF8, 0, adapter->FriendlyName, -1, utf8, sizeof(utf8), nullptr, nullptr);
			label = utf8;
		}
		std::unordered_set<unsigned int> families_seen;
		for (auto *address = adapter->FirstUnicastAddress; address; address = address->Next) {
			if (!address->Address.lpSockaddr)
				continue;
			const auto family = static_cast<unsigned int>(address->Address.lpSockaddr->sa_family);
			if (family != AF_INET && family != AF_INET6)
				continue;
			if (!families_seen.insert(family).second)
				continue;
			std::array<char, INET6_ADDRSTRLEN> text{};
			if (!inet_ntop(family,
					family == AF_INET
						? static_cast<void *>(&reinterpret_cast<sockaddr_in *>(address->Address.lpSockaddr)->sin_addr)
						: static_cast<void *>(&reinterpret_cast<sockaddr_in6 *>(address->Address.lpSockaddr)->sin6_addr),
					text.data(), static_cast<socklen_t>(text.size())))
				continue;
			NetworkAdapter item;
			item.id = std::string(guid) + "/" + std::to_string(family);
			item.label = label.empty() ? item.id : label;
			item.address = text.data();
			item.family = family;
			item.operational = adapter->OperStatus == IfOperStatusUp;
			result_list.push_back(std::move(item));
		}
	}
	return result_list;
}

#else

std::vector<NetworkAdapter> NetworkMonitor::enumerate() const
{
	return {};
}

#endif

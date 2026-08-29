#include "network-monitor.hpp"

#include <arpa/inet.h>
#include <ifaddrs.h>
#include <net/if.h>
#include <netinet/in.h>
#include <sys/socket.h>

#include <array>
#include <cstring>
#include <string>
#include <unordered_set>
#include <utility>

std::vector<NetworkAdapter> NetworkMonitor::enumerate() const
{
	ifaddrs *head = nullptr;
	if (getifaddrs(&head) != 0)
		return {};

	std::vector<NetworkAdapter> result;
	std::unordered_set<std::string> families_seen;
	for (auto *entry = head; entry; entry = entry->ifa_next) {
		if (!entry->ifa_name || !entry->ifa_addr || (entry->ifa_flags & IFF_LOOPBACK))
			continue;
		const int family = entry->ifa_addr->sa_family;
		if (family != AF_INET && family != AF_INET6)
			continue;

		const std::string id = std::string(entry->ifa_name) + "/" + std::to_string(family);
		if (!families_seen.insert(id).second)
			continue;

		const void *address = nullptr;
		if (family == AF_INET) {
			address = &reinterpret_cast<const sockaddr_in *>(entry->ifa_addr)->sin_addr;
		} else {
			address = &reinterpret_cast<const sockaddr_in6 *>(entry->ifa_addr)->sin6_addr;
		}
		std::array<char, INET6_ADDRSTRLEN> text{};
		if (!inet_ntop(family, address, text.data(), static_cast<socklen_t>(text.size())))
			continue;

		NetworkAdapter item;
		item.id = id;
		item.label = entry->ifa_name;
		item.address = text.data();
		item.family = static_cast<unsigned int>(family);
		item.operational = (entry->ifa_flags & IFF_UP) != 0;
		result.push_back(std::move(item));
	}
	freeifaddrs(head);
	return result;
}

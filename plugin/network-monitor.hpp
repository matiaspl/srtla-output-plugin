#pragma once

#include <string>
#include <vector>

struct NetworkAdapter {
	std::string id;
	std::string label;
	std::string address;
	unsigned int family = 0;
	bool enabled = false;
	bool operational = false;
};

class NetworkMonitor {
public:
	std::vector<NetworkAdapter> enumerate() const;
};

#include "passphrase-store.hpp"

std::string store_srtla_passphrase(std::string_view plaintext)
{
	return std::string(plaintext);
}

std::string load_srtla_passphrase(std::string_view stored)
{
	return std::string(stored);
}

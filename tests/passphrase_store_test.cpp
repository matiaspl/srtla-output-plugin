#include "../plugin/passphrase-store.hpp"

#include <cassert>
#include <string>

int main()
{
	const std::string passphrase = "poprawne haslo";
	const auto stored = store_srtla_passphrase(passphrase);
	assert(stored == passphrase);
	assert(load_srtla_passphrase(stored) == passphrase);
	assert(store_srtla_passphrase("").empty());
	assert(load_srtla_passphrase("").empty());
	return 0;
}

#include "../plugin/secret-store.hpp"

#include <cassert>
#include <string>

int main()
{
	const std::string secret = "a-valid-passphrase";
	const auto protected_secret = protect_srtla_secret(secret);
#ifdef _WIN32
	assert(!protected_secret.empty());
	assert(protected_secret != secret);
	assert(unprotect_srtla_secret(protected_secret) == secret);
	assert(unprotect_srtla_secret("not-a-dpapi-blob").empty());
#else
	// Non-Windows builds must not silently downgrade to plaintext storage.
	assert(protected_secret.empty());
#endif
	assert(protect_srtla_secret("").empty());
	return 0;
}

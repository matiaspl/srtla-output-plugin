#pragma once

#include <string>
#include <string_view>

// Windows stores the passphrase as a DPAPI blob tied to the current user.
// Non-Windows builds keep the same interface for tests and return the input;
// those platforms are intentionally outside the v1 packaging target.
std::string protect_srtla_secret(std::string_view plaintext);
std::string unprotect_srtla_secret(std::string_view encoded);


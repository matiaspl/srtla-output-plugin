#pragma once

#include <string>
#include <string_view>

// Windows stores the passphrase as a DPAPI blob tied to the current user.
// Non-Windows builds reject non-empty secrets instead of silently persisting
// them in plaintext; those platforms are intentionally outside v1 packaging.
std::string protect_srtla_secret(std::string_view plaintext);
std::string unprotect_srtla_secret(std::string_view encoded);

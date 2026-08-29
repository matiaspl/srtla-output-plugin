#pragma once

#include <string>
#include <string_view>

// Passphrases are confidential configuration, but are intentionally not
// treated as OS-managed secrets. These helpers keep the storage policy
// explicit and provide a small seam for the profile/configuration tests.
std::string store_srtla_passphrase(std::string_view plaintext);
std::string load_srtla_passphrase(std::string_view stored);

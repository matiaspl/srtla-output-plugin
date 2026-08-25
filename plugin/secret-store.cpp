#include "secret-store.hpp"

#ifdef _WIN32
#ifndef WIN32_LEAN_AND_MEAN
#define WIN32_LEAN_AND_MEAN
#endif
#include <windows.h>
#include <wincrypt.h>

#include <cstdint>
#include <vector>

namespace {
constexpr char alphabet[] = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

std::string base64_encode(const BYTE *data, DWORD size)
{
	std::string out;
	for (DWORD i = 0; i < size; i += 3) {
		const DWORD remaining = size - i;
		const std::uint32_t value = (static_cast<std::uint32_t>(data[i]) << 16) |
			(remaining > 1 ? static_cast<std::uint32_t>(data[i + 1]) << 8 : 0) |
			(remaining > 2 ? data[i + 2] : 0);
		out.push_back(alphabet[(value >> 18) & 63]);
		out.push_back(alphabet[(value >> 12) & 63]);
		out.push_back(remaining > 1 ? alphabet[(value >> 6) & 63] : '=');
		out.push_back(remaining > 2 ? alphabet[value & 63] : '=');
	}
	return out;
}

int decode_char(char c)
{
	if (c >= 'A' && c <= 'Z') return c - 'A';
	if (c >= 'a' && c <= 'z') return c - 'a' + 26;
	if (c >= '0' && c <= '9') return c - '0' + 52;
	if (c == '+') return 62;
	if (c == '/') return 63;
	return -1;
}

std::vector<BYTE> base64_decode(std::string_view encoded)
{
	std::vector<BYTE> out;
	for (std::size_t i = 0; i + 3 < encoded.size(); i += 4) {
		const int a = decode_char(encoded[i]);
		const int b = decode_char(encoded[i + 1]);
		const int c = encoded[i + 2] == '=' ? 0 : decode_char(encoded[i + 2]);
		const int d = encoded[i + 3] == '=' ? 0 : decode_char(encoded[i + 3]);
		if (a < 0 || b < 0 || c < 0 || d < 0) return {};
		const std::uint32_t value = (a << 18) | (b << 12) | (c << 6) | d;
		out.push_back(static_cast<BYTE>(value >> 16));
		if (encoded[i + 2] != '=') out.push_back(static_cast<BYTE>(value >> 8));
		if (encoded[i + 3] != '=') out.push_back(static_cast<BYTE>(value));
	}
	return out;
}
} // namespace

std::string protect_srtla_secret(std::string_view plaintext)
{
	DATA_BLOB input{static_cast<DWORD>(plaintext.size()), reinterpret_cast<BYTE *>(const_cast<char *>(plaintext.data()))};
	DATA_BLOB output{};
	if (plaintext.empty() || !CryptProtectData(&input, L"OBS SRTLA", nullptr, nullptr, nullptr, 0, &output))
		return {};
	const auto encoded = base64_encode(output.pbData, output.cbData);
	LocalFree(output.pbData);
	return encoded;
}

std::string unprotect_srtla_secret(std::string_view encoded)
{
	const auto bytes = base64_decode(encoded);
	if (bytes.empty()) return {};
	DATA_BLOB input{static_cast<DWORD>(bytes.size()), const_cast<BYTE *>(bytes.data())};
	DATA_BLOB output{};
	if (!CryptUnprotectData(&input, nullptr, nullptr, nullptr, nullptr, 0, &output))
		return {};
	std::string plaintext(reinterpret_cast<char *>(output.pbData), output.cbData);
	LocalFree(output.pbData);
	return plaintext;
}

#else

std::string protect_srtla_secret(std::string_view plaintext) { return std::string(plaintext); }
std::string unprotect_srtla_secret(std::string_view encoded) { return std::string(encoded); }

#endif

#pragma once

#include <cstddef>

#include <QByteArray>
#include <QJsonDocument>
#include <QJsonParseError>

namespace srtla {

// The engine's C ABI reports and writes the JSON document plus one terminating
// NUL byte. Keep that byte available to the ABI while excluding it from Qt's
// length-delimited JSON input.
inline QJsonDocument parse_stats_json(const QByteArray &buffer, std::size_t required,
				      QJsonParseError *error = nullptr)
{
	if (required == 0 || required > static_cast<std::size_t>(buffer.size()))
		return {};

	const auto terminator = static_cast<qsizetype>(required - 1);
	if (buffer.at(terminator) != '\0')
		return {};

	return QJsonDocument::fromJson(QByteArray(buffer.constData(), terminator), error);
}

} // namespace srtla

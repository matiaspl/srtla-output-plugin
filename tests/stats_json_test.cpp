#include "stats-json.hpp"
#include "../engine/include/srtla_engine.h"

#include <cstddef>
#include <iostream>

#include <QJsonArray>
#include <QJsonObject>

static_assert(sizeof(SrtlaSrtStats) == 88);
static_assert(offsetof(SrtlaSrtStats, rtt_ms) == 72);
static_assert(offsetof(SrtlaSrtStats, send_buffer_packets) == 76);
static_assert(offsetof(SrtlaSrtStats, latency_ms) == 80);

int main()
{
	const auto require = [](bool condition, const char *message) {
		if (!condition)
			std::cerr << message << '\n';
		return condition;
	};

	QByteArray buffer(R"({"link_capacity_bps":6000000,"srt_send_buffer_packets":12,"srt_rtt_ms":84,"srt_latency_ms":2000,"abr_state":"Hold","recommended_video_bps":4000000,"srt_stats_ready":true,"links":[{"target_bps":4000000,"delivered_bps":3500000}]})");
	buffer.append('\0');
	const auto actual = static_cast<std::size_t>(buffer.size());
	// Simulate a document that became shorter between the ABI size query and
	// copy: the allocation is larger, but `actual` identifies the fresh NUL.
	buffer.append(16, '\0');

	QJsonParseError error{};
	const auto document = srtla::parse_stats_json(buffer, actual, &error);
	if (!require(error.error == QJsonParseError::NoError, "terminated stats JSON did not parse") ||
	    !require(document.isObject(), "stats JSON root is not an object"))
		return 1;
	const auto root = document.object();
	const auto links = root.value(QStringLiteral("links")).toArray();
	if (!require(root.value(QStringLiteral("link_capacity_bps")).toInteger() == 6'000'000,
		     "stats JSON link capacity changed during parsing") ||
	    !require(root.value(QStringLiteral("srt_send_buffer_packets")).toInteger() == 12,
		     "stats JSON queue depth changed during parsing") ||
	    !require(root.value(QStringLiteral("srt_rtt_ms")).toInteger() == 84,
		     "stats JSON RTT changed during parsing") ||
	    !require(root.value(QStringLiteral("srt_latency_ms")).toInteger() == 2'000,
		     "stats JSON latency changed during parsing") ||
	    !require(root.value(QStringLiteral("abr_state")).toString() == QStringLiteral("Hold"),
		     "stats JSON ABR state changed during parsing") ||
	    !require(root.value(QStringLiteral("recommended_video_bps")).toInteger() == 4'000'000,
		     "stats JSON ABR target changed during parsing") ||
	    !require(root.value(QStringLiteral("srt_stats_ready")).toBool(),
		     "stats JSON SRT readiness changed during parsing") ||
	    !require(links.size() == 1, "stats JSON link array has the wrong size") ||
	    !require(links.at(0).toObject().value(QStringLiteral("target_bps")).toInteger() == 4'000'000,
		     "stats JSON CC target changed during parsing") ||
	    !require(links.at(0).toObject().value(QStringLiteral("delivered_bps")).toInteger() == 3'500'000,
		     "stats JSON delivered rate changed during parsing"))
		return 1;

	QByteArray unterminated(R"({"links":[]})");
	if (!require(srtla::parse_stats_json(unterminated, static_cast<std::size_t>(unterminated.size())).isNull(),
		     "unterminated ABI buffer was accepted"))
		return 1;
	return 0;
}

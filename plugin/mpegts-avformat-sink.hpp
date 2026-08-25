#pragma once

#include <cstddef>
#include <cstdint>
#include <functional>
#include <string>
#include <vector>

#include <libavformat/version.h>

struct encoder_packet;
struct obs_output;

/**
 * Thin OBS-to-libavformat MPEG-TS adapter.
 *
 * The class deliberately does not implement PAT/PMT/PES or TS continuity
 * counters.  libavformat owns the container state; the only plugin-specific
 * part is the AVIO sink which turns muxed bytes into bounded transport
 * datagrams for the embedded SRTLA pipeline.
 */
class MpegTsAvformatSink final {
public:
	using DatagramCallback = std::function<void(std::vector<std::uint8_t>, bool keyframe, std::int64_t pts90k)>;

	MpegTsAvformatSink(struct obs_output *output, DatagramCallback callback);
	~MpegTsAvformatSink();

	MpegTsAvformatSink(const MpegTsAvformatSink &) = delete;
	MpegTsAvformatSink &operator=(const MpegTsAvformatSink &) = delete;

	/** Recreate the format context at the next accepted packet boundary. */
	void reset();
	/** Mux one OBS encoded packet. */
	bool write(const encoder_packet *packet);
	/** Flush libavformat and emit a final partial datagram, if any. */
	void flush();

	bool healthy() const { return healthy_; }
	const std::string &last_error() const { return last_error_; }

private:
	bool ensure_started();
	bool create_streams();
	bool write_header();
	void destroy_context(bool write_trailer = true);
	bool write_packet(const encoder_packet *packet);
	void emit_datagrams(bool flush_partial);
	void set_error(const char *message);

#if LIBAVFORMAT_VERSION_MAJOR >= 61
	static int write_callback(void *opaque, const std::uint8_t *buffer, int size);
#else
	static int write_callback(void *opaque, std::uint8_t *buffer, int size);
#endif

	struct obs_output *output_ = nullptr;
	struct AVFormatContext *format_ = nullptr;
	struct AVIOContext *io_ = nullptr;
	struct AVStream *video_stream_ = nullptr;
	struct AVStream *audio_stream_ = nullptr;
	std::vector<std::uint8_t> pending_;
	DatagramCallback callback_;
	bool header_written_ = false;
	bool healthy_ = true;
	bool current_keyframe_ = false;
	std::int64_t current_pts90k_ = 0;
	std::string last_error_;
};

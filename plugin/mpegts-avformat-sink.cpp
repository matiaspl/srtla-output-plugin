#include "mpegts-avformat-sink.hpp"

#include <obs.h>
#include <obs-encoder.h>
#include <obs-data.h>
#include <media-io/audio-io.h>

extern "C" {
#include <libavcodec/avcodec.h>
#include <libavformat/avformat.h>
#include <libavutil/channel_layout.h>
#include <libavutil/error.h>
#include <libavutil/mem.h>
}

#include <cstring>
#include <limits>
#include <utility>

namespace {
constexpr int AVIO_BUFFER_SIZE = 32 * 1024;
constexpr std::size_t TS_DATAGRAM_SIZE = 1316;

AVRational packet_timebase(const encoder_packet *packet, AVRational fallback)
{
	if (!packet || packet->timebase_num <= 0 || packet->timebase_den <= 0)
		return fallback;
	return AVRational{packet->timebase_num, packet->timebase_den};
}

std::int64_t to_90k(std::int64_t value, AVRational source, AVRational destination)
{
	return av_rescale_q(value, source, destination);
}

enum AVCodecID codec_id_for(const char *codec, bool video)
{
	if (!codec)
		return AV_CODEC_ID_NONE;
	if (video) {
		if (std::strcmp(codec, "h264") == 0)
			return AV_CODEC_ID_H264;
		if (std::strcmp(codec, "hevc") == 0 || std::strcmp(codec, "h265") == 0)
			return AV_CODEC_ID_HEVC;
	} else {
		if (std::strcmp(codec, "aac") == 0)
			return AV_CODEC_ID_AAC;
		if (std::strcmp(codec, "opus") == 0)
			return AV_CODEC_ID_OPUS;
	}
	return AV_CODEC_ID_NONE;
}

bool copy_extradata(AVCodecParameters *parameters, obs_encoder_t *encoder)
{
	std::uint8_t *extra_data = nullptr;
	std::size_t extra_size = 0;
	if (!encoder || !obs_encoder_get_extra_data(encoder, &extra_data, &extra_size) || !extra_data || extra_size == 0)
		return false;
	if (extra_size > static_cast<std::size_t>(std::numeric_limits<int>::max()))
		return false;

	parameters->extradata = static_cast<std::uint8_t *>(av_mallocz(extra_size + AV_INPUT_BUFFER_PADDING_SIZE));
	if (!parameters->extradata)
		return false;
	std::memcpy(parameters->extradata, extra_data, extra_size);
	parameters->extradata_size = static_cast<int>(extra_size);
	return true;
}
} // namespace

MpegTsAvformatSink::MpegTsAvformatSink(struct obs_output *output, DatagramCallback callback)
	: output_(output), callback_(std::move(callback))
{
	pending_.reserve(TS_DATAGRAM_SIZE * 2);
}

MpegTsAvformatSink::~MpegTsAvformatSink()
{
	destroy_context();
}

void MpegTsAvformatSink::set_error(const char *message)
{
	healthy_ = false;
	last_error_ = message ? message : "MPEG-TS muxer error";
}

void MpegTsAvformatSink::destroy_context(bool write_trailer)
{
	if (format_) {
		if (write_trailer && header_written_)
			av_write_trailer(format_);
		format_->pb = nullptr;
		avformat_free_context(format_);
		format_ = nullptr;
	}
	if (io_) {
		// avformat_free_context does not own custom AVIO buffers when pb is
		// detached.  avio_context_free releases both the context and buffer.
		avio_context_free(&io_);
	}
	video_stream_ = nullptr;
	audio_stream_ = nullptr;
	header_written_ = false;
	pending_.clear();
}

void MpegTsAvformatSink::reset()
{
	// A reconnect starts a new MPEG-TS section at the next keyframe.  Do not
	// write an old context's trailer into the new transport queue.
	destroy_context(false);
	healthy_ = true;
	last_error_.clear();
	current_keyframe_ = false;
	current_pts90k_ = 0;
}

bool MpegTsAvformatSink::create_streams()
{
	obs_encoder_t *video_encoder = obs_output_get_video_encoder(output_);
	if (!video_encoder)
		return false;

	obs_video_info video_info{};
	if (!obs_get_video_info(&video_info))
		return false;

	const AVCodecID video_codec = codec_id_for(obs_encoder_get_codec(video_encoder), true);
	if (video_codec == AV_CODEC_ID_NONE)
		return false;
	video_stream_ = avformat_new_stream(format_, nullptr);
	if (!video_stream_)
		return false;
	video_stream_->id = 0;
	video_stream_->time_base = AVRational{static_cast<int>(video_info.fps_den), static_cast<int>(video_info.fps_num)};
	video_stream_->avg_frame_rate = AVRational{static_cast<int>(video_info.fps_num), static_cast<int>(video_info.fps_den)};
	video_stream_->codecpar->codec_type = AVMEDIA_TYPE_VIDEO;
	video_stream_->codecpar->codec_id = video_codec;
	video_stream_->codecpar->width = static_cast<int>(obs_output_get_width(output_));
	video_stream_->codecpar->height = static_cast<int>(obs_output_get_height(output_));
	if (obs_data_t *settings = obs_encoder_get_settings(video_encoder)) {
		video_stream_->codecpar->bit_rate = obs_data_get_int(settings, "bitrate") * 1000;
		obs_data_release(settings);
	}
	if (!copy_extradata(video_stream_->codecpar, video_encoder))
		return false;

	obs_encoder_t *audio_encoder = obs_output_get_audio_encoder(output_, 0);
	if (audio_encoder) {
		const AVCodecID audio_codec = codec_id_for(obs_encoder_get_codec(audio_encoder), false);
		if (audio_codec == AV_CODEC_ID_NONE)
			return false;
		audio_t *audio = obs_get_audio();
		const int sample_rate = audio ? static_cast<int>(audio_output_get_sample_rate(audio)) : 48000;
		const int channels = audio ? static_cast<int>(audio_output_get_channels(audio)) : 2;
		if (sample_rate <= 0 || channels <= 0)
			return false;

		audio_stream_ = avformat_new_stream(format_, nullptr);
		if (!audio_stream_)
			return false;
		audio_stream_->id = 1;
		audio_stream_->time_base = AVRational{1, sample_rate};
		audio_stream_->codecpar->codec_type = AVMEDIA_TYPE_AUDIO;
		audio_stream_->codecpar->codec_id = audio_codec;
		audio_stream_->codecpar->sample_rate = sample_rate;
		av_channel_layout_default(&audio_stream_->codecpar->ch_layout, channels);
		audio_stream_->codecpar->format = audio_codec == AV_CODEC_ID_OPUS ? AV_SAMPLE_FMT_FLT : AV_SAMPLE_FMT_FLTP;
		if (obs_data_t *settings = obs_encoder_get_settings(audio_encoder)) {
			audio_stream_->codecpar->bit_rate = obs_data_get_int(settings, "bitrate") * 1000;
			obs_data_release(settings);
		}
		if (!copy_extradata(audio_stream_->codecpar, audio_encoder))
			return false;
	}

	return true;
}

bool MpegTsAvformatSink::write_header()
{
	if (!format_ || !video_stream_)
		return false;

	// OBS's MPEG-TS output uses the same flag to let libavformat insert the
	// required H.264/HEVC bitstream filter when encoder packets and extradata
	// use different representations.
	format_->flags |= AVFMT_FLAG_AUTO_BSF;
	const int result = avformat_write_header(format_, nullptr);
	if (result < 0) {
		char error[AV_ERROR_MAX_STRING_SIZE]{};
		av_strerror(result, error, sizeof(error));
		set_error(error);
		return false;
	}
	header_written_ = true;
	return true;
}

bool MpegTsAvformatSink::ensure_started()
{
	if (header_written_)
		return healthy_;
	if (!output_) {
		set_error("OBS output is unavailable");
		return false;
	}

	destroy_context();
	const AVOutputFormat *output_format = av_guess_format("mpegts", nullptr, "video/MP2T");
	if (!output_format) {
		set_error("FFmpeg MPEG-TS format is unavailable");
		return false;
	}
	if (avformat_alloc_output_context2(&format_, output_format, nullptr, nullptr) < 0 || !format_) {
		set_error("Could not allocate FFmpeg MPEG-TS context");
		return false;
	}

	unsigned char *buffer = static_cast<unsigned char *>(av_malloc(AVIO_BUFFER_SIZE));
	if (!buffer) {
		set_error("Could not allocate FFmpeg AVIO buffer");
		destroy_context();
		return false;
	}
	io_ = avio_alloc_context(buffer, AVIO_BUFFER_SIZE, 1, this, nullptr, &MpegTsAvformatSink::write_callback, nullptr);
	if (!io_) {
		av_free(buffer);
		set_error("Could not allocate FFmpeg AVIO context");
		destroy_context();
		return false;
	}
	io_->max_packet_size = static_cast<int>(TS_DATAGRAM_SIZE);
	format_->pb = io_;
	format_->flags |= AVFMT_FLAG_CUSTOM_IO;

	if (!create_streams() || !write_header()) {
		if (healthy_)
			set_error("Could not initialize MPEG-TS streams");
		destroy_context();
		return false;
	}
	return true;
}

#if LIBAVFORMAT_VERSION_MAJOR >= 61
int MpegTsAvformatSink::write_callback(void *opaque, const std::uint8_t *buffer, int size)
#else
int MpegTsAvformatSink::write_callback(void *opaque, std::uint8_t *buffer, int size)
#endif
{
	if (!opaque || !buffer || size <= 0)
		return 0;
	auto *sink = static_cast<MpegTsAvformatSink *>(opaque);
	sink->pending_.insert(sink->pending_.end(), buffer, buffer + size);
	sink->emit_datagrams(false);
	return size;
}

void MpegTsAvformatSink::emit_datagrams(bool flush_partial)
{
	while (pending_.size() >= TS_DATAGRAM_SIZE) {
		std::vector<std::uint8_t> datagram(pending_.begin(), pending_.begin() + TS_DATAGRAM_SIZE);
		pending_.erase(pending_.begin(), pending_.begin() + TS_DATAGRAM_SIZE);
		if (callback_)
			callback_(std::move(datagram), current_keyframe_, current_pts90k_);
	}
	if (flush_partial && !pending_.empty()) {
		std::vector<std::uint8_t> datagram;
		datagram.swap(pending_);
		if (callback_)
			callback_(std::move(datagram), current_keyframe_, current_pts90k_);
	}
}

bool MpegTsAvformatSink::write_packet(const encoder_packet *packet)
{
	AVStream *stream = packet->type == OBS_ENCODER_VIDEO ? video_stream_ : audio_stream_;
	if (!stream || (packet->type == OBS_ENCODER_AUDIO && packet->track_idx != 0))
		return false;
	if (packet->size > static_cast<std::size_t>(std::numeric_limits<int>::max())) {
		set_error("Encoded packet is too large for FFmpeg");
		return false;
	}

	AVPacket *av_packet = av_packet_alloc();
	if (!av_packet || av_new_packet(av_packet, static_cast<int>(packet->size)) < 0) {
		av_packet_free(&av_packet);
		return false;
	}
	std::memcpy(av_packet->data, packet->data, packet->size);
	av_packet->stream_index = stream->index;
	const AVRational source_timebase = packet_timebase(packet, stream->time_base);
	av_packet->pts = packet->pts == AV_NOPTS_VALUE ? AV_NOPTS_VALUE :
		av_rescale_q(packet->pts, source_timebase, stream->time_base);
	av_packet->dts = packet->dts == AV_NOPTS_VALUE ? AV_NOPTS_VALUE :
		av_rescale_q(packet->dts, source_timebase, stream->time_base);
	if (packet->keyframe)
		av_packet->flags |= AV_PKT_FLAG_KEY;

	current_keyframe_ = packet->keyframe;
	current_pts90k_ = av_packet->pts == AV_NOPTS_VALUE ? 0 :
		to_90k(av_packet->pts, stream->time_base, AVRational{1, 90000});
	const int result = av_interleaved_write_frame(format_, av_packet);
	current_keyframe_ = false;
	av_packet_free(&av_packet);
	if (result < 0) {
		char error[AV_ERROR_MAX_STRING_SIZE]{};
		av_strerror(result, error, sizeof(error));
		set_error(error);
		return false;
	}
	return true;
}

bool MpegTsAvformatSink::write(const encoder_packet *packet)
{
	if (!packet || !packet->data || packet->size == 0 || !healthy_)
		return false;
	if (!ensure_started())
		return false;
	return write_packet(packet);
}

void MpegTsAvformatSink::flush()
{
	if (!format_ || !io_ || !healthy_)
		return;
	avio_flush(io_);
	emit_datagrams(true);
}

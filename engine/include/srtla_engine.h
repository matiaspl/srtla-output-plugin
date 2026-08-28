#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct SrtlaEngineHandle SrtlaEngineHandle;

typedef enum SrtlaSessionState {
	SRTLA_SESSION_IDLE = 0,
	SRTLA_SESSION_CONNECTING = 1,
	SRTLA_SESSION_CONNECTED = 2,
	SRTLA_SESSION_RECONNECTING = 3,
	SRTLA_SESSION_FATAL = 4,
	SRTLA_SESSION_STOPPED = 5,
} SrtlaSessionState;

/* SRT feedback used by the BELABOX-style encoder controller. */
typedef struct SrtlaSrtStats {
	uint32_t struct_size;
	uint32_t reserved;
	uint64_t sampled_at_ms;
	uint64_t bandwidth_bps;
	uint64_t send_rate_bps;
	uint64_t sent_unique_bytes;
	uint64_t retransmitted_bytes;
	uint64_t dropped_bytes;
	uint32_t send_buffer_ms;
	uint32_t packets_in_flight;
	uint32_t sender_loss_packets;
	uint32_t reserved2;
	uint32_t rtt_ms;
	uint32_t send_buffer_packets;
	uint32_t latency_ms;
	uint32_t reserved3;
} SrtlaSrtStats;

/* The queue and control contract is stable. A successful submit is
 * acceptance into the bounded in-process SRTLA queue, not receiver delivery. */
/* Return codes: 0 success, -1 invalid argument, -2 bounded queue/JSON error,
 * -3 last-link guard or short receive buffer, -4 engine not running. */

/* JSON is accepted as either an array of {id,label,enabled} links or
 * {"links":[...]}. New adapter IDs are disabled until explicitly enabled. */
SrtlaEngineHandle *srtla_engine_create(const char *config_json);
int32_t srtla_engine_start(SrtlaEngineHandle *handle);
int32_t srtla_engine_stop(SrtlaEngineHandle *handle);
void srtla_engine_destroy(SrtlaEngineHandle *handle);
int32_t srtla_engine_submit_srt_datagram(SrtlaEngineHandle *handle, const uint8_t *data, size_t len);
int32_t srtla_engine_receive_srt_datagram(SrtlaEngineHandle *handle, uint8_t *data, size_t capacity, uint32_t timeout_ms);
int32_t srtla_engine_set_link_enabled(SrtlaEngineHandle *handle, uint64_t link_id, bool enabled);
int32_t srtla_engine_update_adapters(SrtlaEngineHandle *handle, const char *adapters_json);
int32_t srtla_engine_update_link_stats(SrtlaEngineHandle *handle, const char *stats_json);
int32_t srtla_engine_update_srt_stats(SrtlaEngineHandle *handle, const SrtlaSrtStats *stats);
int32_t srtla_engine_set_audio_bitrate(SrtlaEngineHandle *handle, uint64_t audio_bps);
int32_t srtla_engine_set_video_bitrate(SrtlaEngineHandle *handle, uint64_t video_bps);
int32_t srtla_engine_set_max_video_bitrate(SrtlaEngineHandle *handle, uint64_t max_video_bps);
int32_t srtla_engine_set_session_state(SrtlaEngineHandle *handle, SrtlaSessionState state,
						       const char *error);
uint64_t srtla_engine_apply_abr(SrtlaEngineHandle *handle, uint64_t now_ms);
size_t srtla_engine_copy_stats_json(SrtlaEngineHandle *handle, char *output, size_t capacity);
const char *srtla_engine_last_error(SrtlaEngineHandle *handle);

#ifdef __cplusplus
}
#endif

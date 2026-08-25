#pragma once

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct SrtlaEngineHandle SrtlaEngineHandle;

/* v1 ABI: the queue and control contract is stable.  A successful submit is
 * acceptance into the bounded in-process SRTLA queue, not receiver delivery. */
/* Return codes: 0 success, -1 invalid argument, -2 bounded queue/JSON error,
 * -3 last-link guard or short receive buffer, -4 engine not running. */

/* JSON is accepted as either an array of {id,label,enabled} links or
 * {"links":[...]}. New adapter IDs are disabled until explicitly enabled. */
SrtlaEngineHandle *srtla_engine_create_v1(const char *config_json);
int32_t srtla_engine_start(SrtlaEngineHandle *handle);
int32_t srtla_engine_stop(SrtlaEngineHandle *handle);
void srtla_engine_destroy(SrtlaEngineHandle *handle);
int32_t srtla_engine_submit_srt_datagram(SrtlaEngineHandle *handle, const uint8_t *data, size_t len);
int32_t srtla_engine_receive_srt_datagram(SrtlaEngineHandle *handle, uint8_t *data, size_t capacity, uint32_t timeout_ms);
int32_t srtla_engine_set_link_enabled(SrtlaEngineHandle *handle, uint64_t link_id, bool enabled);
int32_t srtla_engine_update_adapters(SrtlaEngineHandle *handle, const char *adapters_json);
int32_t srtla_engine_update_link_stats(SrtlaEngineHandle *handle, const char *stats_json);
int32_t srtla_engine_set_audio_bitrate(SrtlaEngineHandle *handle, uint64_t audio_bps);
int32_t srtla_engine_set_video_bitrate(SrtlaEngineHandle *handle, uint64_t video_bps);
uint64_t srtla_engine_apply_abr(SrtlaEngineHandle *handle, uint64_t now_ms);
size_t srtla_engine_copy_stats_json(SrtlaEngineHandle *handle, char *output, size_t capacity);
const char *srtla_engine_last_error(SrtlaEngineHandle *handle);

#ifdef __cplusplus
}
#endif

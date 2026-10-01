#pragma once

class SrtlaDock;

bool srtla_websocket_initialize(SrtlaDock *dock);
void srtla_websocket_shutdown();
void srtla_websocket_emit_status_json(const char *json);

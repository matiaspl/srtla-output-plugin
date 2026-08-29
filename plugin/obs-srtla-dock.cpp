#include "obs-srtla-dock.hpp"
#include "stats-json.hpp"

#include "network-monitor.hpp"
#include "passphrase-store.hpp"

#include <obs.h>
#include <obs-encoder.h>
#include <obs-frontend-api.h>
#include <util/config-file.h>

#include <QCheckBox>
#include <QAbstractItemView>
#include <QComboBox>
#include <QFormLayout>
#include <QHeaderView>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QLineEdit>
#include <QSignalBlocker>
#include <QPushButton>
#include <QUrl>
#include <QUrlQuery>
#include <QSpinBox>
#include <QTableWidget>
#include <QTableWidgetItem>
#include <QTimer>
#include <QVBoxLayout>

#include <cstddef>
#include <cstdint>
#include <cstring>

extern "C" std::uint64_t srtla_output_link_id(const char *adapter_id);
extern "C" int srtla_output_set_link_enabled(struct obs_output *output, std::uint64_t link_id, bool enabled);
extern "C" int srtla_output_set_bitrate_control(struct obs_output *output, bool automatic,
	                                            int manual_bitrate_kbps);
extern "C" int srtla_output_set_max_bitrate(struct obs_output *output, int max_bitrate_kbps);
extern "C" int srtla_output_update_adapters(struct obs_output *output);
extern "C" std::size_t srtla_output_copy_stats_json(struct obs_output *output, char *buffer, std::size_t capacity);

namespace {

constexpr char config_section[] = "SrtlaOutput";

config_t *profile_config()
{
	return obs_frontend_get_profile_config();
}

QString profile_string(const char *name, const QString &fallback = {})
{
	auto *config = profile_config();
	if (!config || !config_has_user_value(config, config_section, name))
		return fallback;
	const char *value = config_get_string(config, config_section, name);
	return value ? QString::fromUtf8(value) : fallback;
}

struct LegacyUrlFields {
	QString endpoint;
	QString stream_id;
	QString passphrase;
	int latency_ms = 2000;
	int pbkeylen = 16;
	bool has_stream_id = false;
	bool has_passphrase = false;
	bool has_latency = false;
	bool has_pbkeylen = false;
};

LegacyUrlFields parse_legacy_url(const QString &raw)
{
	LegacyUrlFields fields;
	QUrl url(raw);
	if (!url.isValid() || url.scheme().isEmpty()) {
		fields.endpoint = raw;
		return fields;
	}
	fields.endpoint = url.toString(QUrl::RemoveQuery | QUrl::RemoveFragment);
	const QUrlQuery query(url);
	for (const auto &[key, value] : query.queryItems(QUrl::FullyDecoded)) {
		if (key == QStringLiteral("streamid") || key == QStringLiteral("stream_id")) {
			fields.stream_id = value;
			fields.has_stream_id = true;
		} else if (key == QStringLiteral("passphrase") || key == QStringLiteral("password")) {
			fields.passphrase = value;
			fields.has_passphrase = true;
		} else if (key == QStringLiteral("latency") || key == QStringLiteral("latency_ms")) {
			bool ok = false;
			const auto parsed = value.toInt(&ok);
			if (ok && parsed >= 120 && parsed <= 60000) {
				fields.latency_ms = parsed;
				fields.has_latency = true;
			}
		} else if (key == QStringLiteral("pbkeylen")) {
			bool ok = false;
			const auto parsed = value.toInt(&ok);
			if (ok && (parsed == 16 || parsed == 24 || parsed == 32)) {
				fields.pbkeylen = parsed;
				fields.has_pbkeylen = true;
			}
		}
	}
	return fields;
}

bool profile_bool(const char *name, bool fallback)
{
	auto *config = profile_config();
	return config && config_has_user_value(config, config_section, name)
		? config_get_bool(config, config_section, name)
		: fallback;
}

int profile_int(const char *name, int fallback)
{
	auto *config = profile_config();
	return config && config_has_user_value(config, config_section, name)
		? static_cast<int>(config_get_int(config, config_section, name))
		: fallback;
}

void save_profile_config()
{
	auto *config = profile_config();
	if (config && config_save_safe(config, "tmp", nullptr) != CONFIG_SUCCESS)
		blog(LOG_WARNING, "[obs-srtla-output] Failed to save dock settings");
}

bool is_supported_video_codec(const char *codec)
{
	return codec && (strcmp(codec, "h264") == 0 || strcmp(codec, "hevc") == 0);
}

bool is_supported_audio_codec(const char *codec)
{
	return codec && (strcmp(codec, "aac") == 0 || strcmp(codec, "opus") == 0);
}

void add_encoder_choices(QComboBox *combo, enum obs_encoder_type type)
{
	for (size_t index = 0;; ++index) {
		const char *id = nullptr;
		if (!obs_enum_encoder_types(index, &id))
			break;
		if (!id || obs_get_encoder_type(id) != type)
			continue;
		const char *codec = obs_get_encoder_codec(id);
		if ((type == OBS_ENCODER_VIDEO && !is_supported_video_codec(codec)) ||
		    (type == OBS_ENCODER_AUDIO && !is_supported_audio_codec(codec)))
			continue;
		const char *display_name = obs_encoder_get_display_name(id);
		combo->addItem(QString::fromUtf8(display_name && *display_name ? display_name : id),
			QString::fromUtf8(id));
	}
}

void select_combo_data(QComboBox *combo, const QString &value)
{
	if (!value.isEmpty()) {
		const int index = combo->findData(value);
		if (index >= 0)
			combo->setCurrentIndex(index);
	}
}

obs_output_t *get_encoder_source_output(const QString &source)
{
	if (source == QStringLiteral("recording"))
		return obs_frontend_get_recording_output();
	return obs_frontend_get_streaming_output();
}

obs_encoder_t *create_custom_video_encoder(const QString &id, int bitrate_kbps)
{
	const QByteArray encoder_id = id.toUtf8();
	obs_data_t *settings = obs_encoder_defaults(encoder_id.constData());
	if (!settings)
		settings = obs_data_create();
	obs_data_set_int(settings, "bitrate", bitrate_kbps);
	obs_encoder_t *encoder = obs_video_encoder_create(encoder_id.constData(), "srtla_custom_video", settings, nullptr);
	obs_data_release(settings);
	if (encoder)
		obs_encoder_set_video(encoder, obs_get_video());
	return encoder;
}

obs_encoder_t *create_custom_audio_encoder(const QString &id, int bitrate_kbps)
{
	const QByteArray encoder_id = id.toUtf8();
	obs_data_t *settings = obs_encoder_defaults(encoder_id.constData());
	if (!settings)
		settings = obs_data_create();
	obs_data_set_int(settings, "bitrate", bitrate_kbps);
	obs_encoder_t *encoder = obs_audio_encoder_create(encoder_id.constData(), "srtla_custom_audio", settings, 0, nullptr);
	obs_data_release(settings);
	if (encoder)
		obs_encoder_set_audio(encoder, obs_get_audio());
	return encoder;
}

} // namespace

SrtlaDock::SrtlaDock(QWidget *parent) : QWidget(parent)
{
	setObjectName(QStringLiteral("SrtlaOutputDock"));
	auto *layout = new QVBoxLayout(this);
	auto *controls = new QFormLayout();
	startStop_ = new QPushButton(tr("Start"), this);
	url_ = new QLineEdit(QStringLiteral("srtla://receiver.example:5000"), this);
	streamId_ = new QLineEdit(this);
	passphrase_ = new QLineEdit(this);
	passphrase_->setEchoMode(QLineEdit::Password);
	secret_warning_ = new QLabel(tr("Passphrase is stored unencrypted in the OBS profile."), this);
	secret_warning_->setWordWrap(true);
	secret_warning_->setStyleSheet(QStringLiteral("color: #b36b00;"));
	encoderSource_ = new QComboBox(this);
	encoderSource_->addItem(tr("Streaming"), QStringLiteral("streaming"));
	encoderSource_->addItem(tr("Recording"), QStringLiteral("recording"));
	encoderSource_->addItem(tr("Custom"), QStringLiteral("custom"));
	customVideoEncoder_ = new QComboBox(this);
	add_encoder_choices(customVideoEncoder_, OBS_ENCODER_VIDEO);
	customAudioEncoder_ = new QComboBox(this);
	add_encoder_choices(customAudioEncoder_, OBS_ENCODER_AUDIO);
	autoBitrate_ = new QCheckBox(tr("Auto bitrate"), this);
	autoBitrate_->setChecked(true);
	manualBitrate_ = new QSpinBox(this);
	manualBitrate_->setRange(500, 100000);
	manualBitrate_->setValue(1500);
	manualBitrate_->setSuffix(tr(" kb/s"));
	manualBitrate_->setEnabled(false);
	maxBitrate_ = new QSpinBox(this);
	maxBitrate_->setRange(500, 30000);
	maxBitrate_->setValue(6000);
	maxBitrate_->setSingleStep(50);
	maxBitrate_->setSuffix(tr(" kb/s"));
	state_ = new QLabel(tr("Idle"), this);
	metrics_ = new QLabel(tr("ABR") + QStringLiteral(": -- | ") + tr("RTT") +
		QStringLiteral(": -- | ") + tr("SRT queue") + QStringLiteral(": --\n") +
		tr("Video") + QStringLiteral(": -- | ") + tr("Target") +
		QStringLiteral(": -- | ") + tr("Active links") + QStringLiteral(": 0"), this);
	loadProfile();
	connect(passphrase_, &QLineEdit::textChanged, this, [this] {
		if (!secret_error_)
			return;
		secret_error_ = false;
		secret_warning_->setText(tr("Passphrase is stored unencrypted in the OBS profile."));
	});
	controls->addRow(tr("State"), state_);
	controls->addRow(tr("SRTLA URL"), url_);
	controls->addRow(tr("Stream ID"), streamId_);
	controls->addRow(tr("Passphrase"), passphrase_);
	controls->addRow(QString(), secret_warning_);
	controls->addRow(tr("Encoder"), encoderSource_);
	controls->addRow(tr("Custom video encoder"), customVideoEncoder_);
	controls->addRow(tr("Custom audio encoder"), customAudioEncoder_);
	controls->addRow(tr("Bitrate"), autoBitrate_);
	controls->addRow(tr("Manual"), manualBitrate_);
	controls->addRow(tr("Auto maximum"), maxBitrate_);
	layout->addLayout(controls);
	layout->addWidget(metrics_);
	layout->addWidget(startStop_);

	links_ = new QTableWidget(this);
	links_->setColumnCount(9);
	links_->setHorizontalHeaderLabels({tr("Use"), tr("Adapter / IP"), tr("State"), tr("NAK score"), tr("RTT"), tr("NAK rate"), tr("Offered"), tr("Delivered"), tr("CC target")});
	links_->horizontalHeaderItem(3)->setToolTip(tr("NAK score tooltip"));
	links_->horizontalHeaderItem(5)->setToolTip(tr("NAK rate tooltip"));
	links_->horizontalHeader()->setStretchLastSection(true);
	links_->setEditTriggers(QAbstractItemView::NoEditTriggers);
	connect(links_, &QTableWidget::itemChanged, this, [this](QTableWidgetItem *item) {
		if (!item || item->column() != 0)
			return;
		const auto *id_item = links_->item(item->row(), 1);
		if (!id_item)
			return;
		const auto key = id_item->data(Qt::UserRole).toString().toStdString();
		const bool enabled = item->checkState() == Qt::Checked;
		if (output_ && running_ && srtla_output_set_link_enabled(output_, srtla_output_link_id(key.c_str()), enabled) != 0) {
			QSignalBlocker blocker(links_);
			item->setCheckState(enabled ? Qt::Unchecked : Qt::Checked);
			return;
		}
		selected_[key] = enabled;
		saveSelectedLinks();
	});
	layout->addWidget(links_, 1);

	connect(startStop_, &QPushButton::clicked, this, &SrtlaDock::toggleOutput);
	connect(autoBitrate_, &QCheckBox::toggled, this, [this](bool automatic) {
		manualBitrate_->setDisabled(automatic);
		if (settings_) {
			obs_data_set_bool(settings_, "auto_bitrate", automatic);
			obs_data_set_int(settings_, "bitrate", manualBitrate_->value());
		}
		if (!running_ || !output_)
			return;
		if (srtla_output_set_bitrate_control(output_, automatic, manualBitrate_->value()) == 0)
			return;
		{
			QSignalBlocker blocker(autoBitrate_);
			autoBitrate_->setChecked(!automatic);
		}
		manualBitrate_->setDisabled(autoBitrate_->isChecked());
		if (settings_)
			obs_data_set_bool(settings_, "auto_bitrate", autoBitrate_->isChecked());
		state_->setToolTip(tr("The selected encoder cannot change bitrate while active"));
	});
	connect(manualBitrate_, QOverload<int>::of(&QSpinBox::valueChanged), this, [this](int bitrate_kbps) {
		if (settings_)
			obs_data_set_int(settings_, "bitrate", bitrate_kbps);
		if (!running_ || !output_ || autoBitrate_->isChecked())
			return;
		if (srtla_output_set_bitrate_control(output_, false, bitrate_kbps) != 0)
			state_->setToolTip(tr("The selected encoder cannot change bitrate while active"));
	});
	connect(maxBitrate_, QOverload<int>::of(&QSpinBox::valueChanged), this, [this](int bitrate_kbps) {
		if (running_ && output_ && srtla_output_set_max_bitrate(output_, bitrate_kbps) != 0) {
			const int previous = settings_ ? static_cast<int>(obs_data_get_int(settings_, "max_bitrate")) : 6000;
			QSignalBlocker blocker(maxBitrate_);
			maxBitrate_->setValue(previous);
			state_->setToolTip(tr("The selected encoder cannot change bitrate while active"));
			return;
		}
		if (settings_)
			obs_data_set_int(settings_, "max_bitrate", bitrate_kbps);
	});
	connect(encoderSource_, QOverload<int>::of(&QComboBox::currentIndexChanged), this, [this](int) {
		const bool custom = encoderSource_->currentData().toString() == QStringLiteral("custom");
		customVideoEncoder_->setEnabled(custom);
		customAudioEncoder_->setEnabled(custom);
	});
	manualBitrate_->setEnabled(!autoBitrate_->isChecked());
	const bool custom = encoderSource_->currentData().toString() == QStringLiteral("custom");
	customVideoEncoder_->setEnabled(custom);
	customAudioEncoder_->setEnabled(custom);
	timer_ = new QTimer(this);
	timer_->setInterval(1000);
	connect(timer_, &QTimer::timeout, this, [this] {
		refreshAdapters();
		refreshStatus();
	});
	refreshAdapters();
	timer_->start();
}

SrtlaDock::~SrtlaDock()
{
	stopOutput();
	if (output_)
		obs_output_release(output_);
	if (settings_)
		obs_data_release(settings_);
}

void SrtlaDock::loadProfile()
{
	secret_error_ = false;
	selected_.clear();
	const auto raw_url = profile_string("Url", QStringLiteral("srtla://receiver.example:5000"));
	const auto legacy = parse_legacy_url(raw_url);
	url_->setText(legacy.endpoint.isEmpty() ? raw_url : legacy.endpoint);
	latency_ms_ = profile_int("LatencyMs", legacy.has_latency ? legacy.latency_ms : 2000);
	pbkeylen_ = profile_int("Pbkeylen", legacy.has_pbkeylen ? legacy.pbkeylen : 16);
	if (legacy.has_latency)
		latency_ms_ = legacy.latency_ms;
	if (legacy.has_pbkeylen)
		pbkeylen_ = legacy.pbkeylen;
	QString stream_id = profile_string("StreamId");
	if (stream_id.isEmpty() && legacy.has_stream_id)
		stream_id = legacy.stream_id;
	streamId_->setText(stream_id);

	const auto stored_secret = profile_string("PassphraseSecret");
	const auto legacy_dpapi = profile_string("PassphraseDpapi");
	QString passphrase = QString::fromStdString(load_srtla_passphrase(stored_secret.toStdString()));
	if (!legacy_dpapi.isEmpty() && stored_secret.isEmpty()) {
		// DPAPI was intentionally removed. Do not attempt to decrypt a value
		// whose availability and semantics depend on another Windows account.
		secret_error_ = true;
		secret_warning_->setText(tr("This profile contains an unsupported encrypted passphrase. Enter it again."));
	} else if (passphrase.isEmpty() && legacy.has_passphrase) {
		passphrase = legacy.passphrase;
	}
	passphrase_->setText(passphrase);
	select_combo_data(encoderSource_, profile_string("EncoderSource", QStringLiteral("streaming")));
	select_combo_data(customVideoEncoder_, profile_string("CustomVideoEncoder"));
	select_combo_data(customAudioEncoder_, profile_string("CustomAudioEncoder"));
	autoBitrate_->setChecked(profile_bool("AutoBitrate", true));
	manualBitrate_->setValue(profile_int("Bitrate", 1500));
	int maximum_bitrate = profile_int("MaxBitrate", 6000);
	if (maximum_bitrate == 100000)
		maximum_bitrate = 6000;
	maxBitrate_->setValue(maximum_bitrate);

	const auto enabled_links = profile_string("EnabledLinks").toStdString();
	std::size_t start = 0;
	while (start <= enabled_links.size()) {
		const auto end = enabled_links.find(',', start);
		const auto id = enabled_links.substr(start, end == std::string::npos ? std::string::npos : end - start);
		if (!id.empty())
			selected_[id] = true;
		if (end == std::string::npos)
			break;
		start = end + 1;
	}
	if (legacy.endpoint != raw_url) {
		// Remove legacy query/fragment material even when an old unsupported
		// encrypted passphrase exists. If the passphrase needs re-entry, preserve
		// that state and only clean the URL.
		if (secret_error_) {
			if (auto *config = profile_config()) {
				config_set_string(config, config_section, "Url", legacy.endpoint.toUtf8().constData());
				save_profile_config();
			}
		} else {
			(void)saveProfile();
		}
	}
}

bool SrtlaDock::saveProfile()
{
	auto *config = profile_config();
	if (!config)
		return false;
	const auto parsed = parse_legacy_url(url_->text());
	const auto endpoint = parsed.endpoint.isEmpty() ? url_->text() : parsed.endpoint;
	if (parsed.has_latency)
		latency_ms_ = parsed.latency_ms;
	if (parsed.has_pbkeylen)
		pbkeylen_ = parsed.pbkeylen;
	const auto passphrase = passphrase_->text().toUtf8().toStdString();
	if (!passphrase.empty()) {
		if (passphrase.size() < 10 || passphrase.size() > 79)
			return false;
	}
	config_set_string(config, config_section, "Url", endpoint.toUtf8().constData());
	config_set_string(config, config_section, "StreamId", streamId_->text().toUtf8().constData());
	const auto stored_passphrase = store_srtla_passphrase(passphrase);
	config_set_string(config, config_section, "PassphraseSecret", stored_passphrase.c_str());
	config_set_string(config, config_section, "PassphraseDpapi", "");
	config_set_int(config, config_section, "LatencyMs", latency_ms_);
	config_set_int(config, config_section, "Pbkeylen", pbkeylen_);
	config_set_string(config, config_section, "EncoderSource", encoderSource_->currentData().toString().toUtf8().constData());
	config_set_string(config, config_section, "CustomVideoEncoder", customVideoEncoder_->currentData().toString().toUtf8().constData());
	config_set_string(config, config_section, "CustomAudioEncoder", customAudioEncoder_->currentData().toString().toUtf8().constData());
	config_set_bool(config, config_section, "AutoBitrate", autoBitrate_->isChecked());
	config_set_int(config, config_section, "Bitrate", manualBitrate_->value());
	config_set_int(config, config_section, "MaxBitrate", maxBitrate_->value());
	save_profile_config();
	secret_error_ = false;
	secret_warning_->setText(tr("Passphrase is stored unencrypted in the OBS profile."));
	return true;
}

void SrtlaDock::reloadProfile()
{
	stopOutput();
	if (output_) {
		obs_output_release(output_);
		output_ = nullptr;
	}
	if (settings_) {
		obs_data_release(settings_);
		settings_ = nullptr;
	}
	loadProfile();
	refreshAdapters();
}

bool SrtlaDock::sharesEncoderWith(obs_output_t *other) const
{
	if (!output_ || !other || output_ == other)
		return false;
	const auto *video = obs_output_get_video_encoder(output_);
	const auto *other_video = obs_output_get_video_encoder(other);
	if (video && video == other_video)
		return true;
	const auto *audio = obs_output_get_audio_encoder(output_, 0);
	const auto *other_audio = obs_output_get_audio_encoder(other, 0);
	return audio && audio == other_audio;
}

void SrtlaDock::stopOutput()
{
	running_ = false;
	if (output_ && obs_output_active(output_))
		obs_output_stop(output_);
	// Profile-bound OBS objects must not survive a profile transition (or be
	// reused after a failed start).  Releasing the output also waits for OBS's
	// asynchronous encoder-capture teardown before a new profile is loaded.
	if (output_) {
		obs_output_release(output_);
		output_ = nullptr;
	}
	if (settings_) {
		obs_data_release(settings_);
		settings_ = nullptr;
	}
	if (startStop_) startStop_->setText(tr("Start"));
	if (state_) state_->setText(tr("Idle"));
}

void SrtlaDock::refreshAdapters()
{
	const auto adapters = NetworkMonitor().enumerate();
	QSignalBlocker blocker(links_);
	links_->setRowCount(static_cast<int>(adapters.size()));
	for (int row = 0; row < links_->rowCount(); ++row) {
		const auto &adapter = adapters[static_cast<size_t>(row)];
		const auto selection = selected_.try_emplace(adapter.id, adapter.enabled).first;
		const bool enabled = selection->second;
		auto *use = new QTableWidgetItem();
		use->setCheckState(enabled ? Qt::Checked : Qt::Unchecked);
		links_->setItem(row, 0, use);
		auto *identity = new QTableWidgetItem(QString::fromStdString(adapter.label + " / " + adapter.address));
		identity->setData(Qt::UserRole, QString::fromStdString(adapter.id));
		links_->setItem(row, 1, identity);
		links_->setItem(row, 2, new QTableWidgetItem(adapter.operational ? tr("Ready") : tr("Offline")));
		links_->setItem(row, 3, new QTableWidgetItem(adapter.operational ? tr("Warming") : tr("Offline")));
		links_->setItem(row, 4, new QTableWidgetItem(QStringLiteral("--")));
		links_->setItem(row, 5, new QTableWidgetItem(QStringLiteral("--")));
		links_->setItem(row, 6, new QTableWidgetItem(QStringLiteral("--")));
		links_->setItem(row, 7, new QTableWidgetItem(QStringLiteral("--")));
		links_->setItem(row, 8, new QTableWidgetItem(QStringLiteral("--")));
	}
	if (output_ && running_)
		srtla_output_update_adapters(output_);
}

void SrtlaDock::saveSelectedLinks()
{
	std::string enabled_links;
	for (const auto &[id, enabled] : selected_) {
		if (!enabled)
			continue;
		if (!enabled_links.empty())
			enabled_links += ',';
		enabled_links += id;
	}
	auto *config = profile_config();
	if (!config)
		return;
	config_set_string(config, config_section, "EnabledLinks", enabled_links.c_str());
	save_profile_config();
}

void SrtlaDock::toggleOutput()
{
	if (running_) {
		stopOutput();
		return;
	}

	if (!settings_)
		settings_ = obs_data_create();

	const QString encoder_source = encoderSource_->currentData().toString();
	const bool custom_encoder = encoder_source == QStringLiteral("custom");
	obs_output_t *source_output = nullptr;
	obs_encoder_t *video_encoder = nullptr;
	obs_encoder_t *audio_encoder = nullptr;

	if (!custom_encoder) {
		source_output = get_encoder_source_output(encoder_source);
		if (!source_output) {
			state_->setText(tr("Error: selected OBS output unavailable"));
			return;
		}
		if (obs_output_active(source_output)) {
			obs_output_release(source_output);
			state_->setText(tr("Error: stop the selected OBS output first"));
			return;
		}
		video_encoder = obs_output_get_video_encoder(source_output);
		audio_encoder = obs_output_get_audio_encoder(source_output, 0);
	} else {
		const QString video_id = customVideoEncoder_->currentData().toString();
		if (video_id.isEmpty()) {
			state_->setText(tr("Error: no custom video encoder available"));
			return;
		}
		video_encoder = create_custom_video_encoder(video_id, manualBitrate_->value());
		if (!video_encoder) {
			state_->setText(tr("Error: custom video encoder could not be created"));
			return;
		}
		const QString audio_id = customAudioEncoder_->currentData().toString();
		if (!audio_id.isEmpty())
			audio_encoder = create_custom_audio_encoder(audio_id, 128);
		if (!audio_id.isEmpty() && !audio_encoder) {
			obs_encoder_release(video_encoder);
			state_->setText(tr("Error: custom audio encoder could not be created"));
			return;
		}
	}

	if (!video_encoder || !is_supported_video_codec(obs_encoder_get_codec(video_encoder))) {
		if (custom_encoder && video_encoder)
			obs_encoder_release(video_encoder);
		if (custom_encoder && audio_encoder)
			obs_encoder_release(audio_encoder);
		if (source_output)
			obs_output_release(source_output);
		state_->setText(tr("Error: selected video encoder is not supported"));
		return;
	}
	if (audio_encoder && !is_supported_audio_codec(obs_encoder_get_codec(audio_encoder))) {
		if (custom_encoder)
			obs_encoder_release(video_encoder);
		if (custom_encoder)
			obs_encoder_release(audio_encoder);
		if (source_output)
			obs_output_release(source_output);
		state_->setText(tr("Error: selected audio encoder is not supported"));
		return;
	}

	if (secret_error_) {
		state_->setText(tr("Error: passphrase could not be loaded; enter it again"));
		return;
	}
	if (!saveProfile()) {
		state_->setText(tr("Error: passphrase could not be saved"));
		return;
	}
	obs_data_set_string(settings_, "url", parse_legacy_url(url_->text()).endpoint.toUtf8().constData());
	obs_data_set_string(settings_, "stream_id", streamId_->text().toUtf8().constData());
	obs_data_set_string(settings_, "passphrase_dpapi", "");
	obs_data_set_string(settings_, "passphrase_secret", passphrase_->text().toUtf8().constData());
	obs_data_set_string(settings_, "passphrase", "");
	std::string enabled_links;
	for (const auto &[id, enabled] : selected_) {
		if (enabled) {
			if (!enabled_links.empty())
				enabled_links += ',';
			enabled_links += id;
		}
	}
	obs_data_set_string(settings_, "enabled_links", enabled_links.c_str());
	obs_data_set_bool(settings_, "auto_bitrate", autoBitrate_->isChecked());
	obs_data_set_int(settings_, "bitrate", manualBitrate_->value());
	obs_data_set_int(settings_, "min_bitrate", 500);
	obs_data_set_int(settings_, "max_bitrate", maxBitrate_->value());
	obs_data_set_int(settings_, "audio_bitrate", 128);
	obs_data_set_int(settings_, "latency_ms", latency_ms_);
	obs_data_set_int(settings_, "pbkeylen", pbkeylen_);
	obs_data_set_string(settings_, "scheduler", "enhanced");
	obs_data_set_string(settings_, "encoder_source", encoder_source.toUtf8().constData());
	obs_data_set_string(settings_, "encoder_mode", custom_encoder ? "dedicated" : "shared");
	obs_data_set_string(settings_, "video_codec", obs_encoder_get_codec(video_encoder));
	if (audio_encoder)
		obs_data_set_string(settings_, "audio_codec", obs_encoder_get_codec(audio_encoder));

	if (!output_)
		output_ = obs_output_create("obs_srtla_output", "SRTLA Output", settings_, nullptr);
	if (!output_) {
		if (custom_encoder)
			obs_encoder_release(video_encoder);
		if (custom_encoder && audio_encoder)
			obs_encoder_release(audio_encoder);
		if (source_output)
			obs_output_release(source_output);
		state_->setText(tr("Error: OBS output unavailable"));
		return;
	}
	obs_output_set_video_encoder(output_, video_encoder);
	obs_output_set_audio_encoder(output_, audio_encoder, 0);
	if (custom_encoder)
		obs_encoder_release(video_encoder);
	if (custom_encoder && audio_encoder)
		obs_encoder_release(audio_encoder);
	if (source_output)
		obs_output_release(source_output);
	obs_output_update(output_, settings_);
	if (!obs_output_start(output_)) {
		const char *error = obs_output_get_last_error(output_);
		const QString detail = error && *error ? QString::fromUtf8(error) : tr("output start failed");
		state_->setText(tr("Error: %1").arg(detail));
		return;
	}
	running_ = true;
	startStop_->setText(tr("Stop"));
	state_->setText(tr("Starting"));
}

void SrtlaDock::refreshStatus()
{
	if (!running_ || !output_)
		return;
	const auto required = srtla_output_copy_stats_json(output_, nullptr, 0);
	if (required == 0)
		return;
	QByteArray buffer(static_cast<qsizetype>(required), '\0');
	const auto actual = srtla_output_copy_stats_json(output_, buffer.data(), required);
	// Statistics can change between the size query and the copy. A larger
	// document was truncated, so wait for the next refresh rather than parse it.
	if (actual == 0 || actual > required)
		return;
	QJsonParseError parse_error{};
	const auto document = srtla::parse_stats_json(buffer, actual, &parse_error);
	if (parse_error.error != QJsonParseError::NoError || !document.isObject())
		return;
	const auto root = document.object();
	const auto links = root.value(QStringLiteral("links")).toArray();
	int active = 0;
	quint64 ccTarget = 0;
	quint64 offered = 0;
	quint64 delivered = 0;
	for (const auto &value : links) {
		const auto link = value.toObject();
		if (link.value(QStringLiteral("payload_eligible")).toBool()) {
			++active;
			ccTarget += link.value(QStringLiteral("target_bps")).toInteger();
		}
		offered += link.value(QStringLiteral("used_bps")).toInteger();
		delivered += link.value(QStringLiteral("delivered_bps")).toInteger();
		const auto id = link.value(QStringLiteral("id")).toVariant().toString();
		for (int row = 0; row < links_->rowCount(); ++row) {
			auto *identity = links_->item(row, 1);
			if (!identity || QString::number(static_cast<qulonglong>(srtla_output_link_id(identity->data(Qt::UserRole).toString().toUtf8().constData()))) != id)
				continue;
			links_->item(row, 2)->setText(link.value(QStringLiteral("state")).toString());
			links_->item(row, 3)->setText(QString::number(link.value(QStringLiteral("quality_percent")).toInt()) + QStringLiteral("%"));
			links_->item(row, 4)->setText(QString::number(link.value(QStringLiteral("rtt_ms")).toInt()) + QStringLiteral(" ms"));
			links_->item(row, 5)->setText(QString::number(link.value(QStringLiteral("loss_permille")).toInt() / 10.0, 'f', 1) + QStringLiteral("%"));
			links_->item(row, 6)->setText(QString::number(link.value(QStringLiteral("used_bps")).toInteger() / 1000) + QStringLiteral(" kb/s"));
			links_->item(row, 7)->setText(QString::number(link.value(QStringLiteral("delivered_bps")).toInteger() / 1000) + QStringLiteral(" kb/s"));
			links_->item(row, 8)->setText(QString::number(link.value(QStringLiteral("target_bps")).toInteger() / 1000) + QStringLiteral(" kb/s"));
			identity->setToolTip(tr("CC: %1 | stall events: %2 | NAK: %3 | scheduler eligibility: %4 | reason: %5")
				.arg(link.value(QStringLiteral("cc_state")).toString())
				.arg(link.value(QStringLiteral("stall_gate_events")).toInteger())
				.arg(link.value(QStringLiteral("nak_count")).toInteger())
				.arg(link.value(QStringLiteral("payload_eligible")).toBool() ? tr("eligible") : tr("excluded"))
				.arg(link.value(QStringLiteral("exclusion_reason")).toString()));
			break;
		}
	}
	const auto linkCapacity = root.value(QStringLiteral("link_capacity_bps")).toInteger(ccTarget);
	const auto srtRaw = root.value(QStringLiteral("srt_bandwidth_bps")).toInteger();
	const auto srtQueue = root.value(QStringLiteral("srt_connected")).toBool() ?
		QStringLiteral("%1 packets / %2 ms")
			.arg(root.value(QStringLiteral("srt_send_buffer_packets")).toInteger())
			.arg(root.value(QStringLiteral("srt_send_buffer_ms")).toInteger()) :
		QStringLiteral("--");
	const auto rtt = root.value(QStringLiteral("srt_connected")).toBool() ?
		QStringLiteral("%1 / %2 ms")
			.arg(root.value(QStringLiteral("srt_rtt_ms")).toInteger())
			.arg(root.value(QStringLiteral("srt_latency_ms")).toInteger()) :
		QStringLiteral("--");
	metrics_->setText(tr("ABR") + QStringLiteral(": %1 | ").arg(root.value(QStringLiteral("abr_state")).toString()) +
		tr("RTT") + QStringLiteral(": %1 | ").arg(rtt) + tr("SRT queue") +
		QStringLiteral(": %1\n").arg(srtQueue) + tr("Video") +
		QStringLiteral(": %1 kb/s | ").arg(root.value(QStringLiteral("current_video_bps")).toInteger() / 1000) +
		tr("Target") + QStringLiteral(": %1 kb/s | ").arg(root.value(QStringLiteral("recommended_video_bps")).toInteger() / 1000) +
		tr("Link CC") + QStringLiteral(": %1 kb/s | ").arg(linkCapacity / 1000) +
		tr("Offered") + QStringLiteral(": %1 kb/s | ").arg(offered / 1000) +
		tr("Delivered") + QStringLiteral(": %1 kb/s | ").arg(delivered / 1000) +
		tr("Active links") + QStringLiteral(": %1").arg(active));
	metrics_->setToolTip(tr("Queue thresholds L/H/S: %1/%2/%3 packets | RTT grow below: %4 ms | RTT reduce above: %5 ms | SRT estimate: %6 kb/s | send: %7 kb/s | retransmissions: %8% | sender loss: %9 | dropped: %10 bytes")
		.arg(root.value(QStringLiteral("abr_queue_light_packets")).toInteger())
		.arg(root.value(QStringLiteral("abr_queue_heavy_packets")).toInteger())
		.arg(root.value(QStringLiteral("abr_queue_severe_packets")).toInteger())
		.arg(root.value(QStringLiteral("abr_rtt_increase_below_ms")).toInteger())
		.arg(root.value(QStringLiteral("abr_rtt_decrease_above_ms")).toInteger())
		.arg(srtRaw / 1000)
		.arg(root.value(QStringLiteral("srt_send_rate_bps")).toInteger() / 1000)
		.arg(root.value(QStringLiteral("srt_retransmit_permille")).toInteger() / 10.0, 0, 'f', 1)
		.arg(root.value(QStringLiteral("srt_sender_loss_packets")).toInteger())
		.arg(root.value(QStringLiteral("srt_dropped_bytes")).toInteger()));
	const auto state = root.value(QStringLiteral("state")).toString(active > 0 ? tr("Live") : tr("Waiting for network"));
	state_->setText(state);
	const auto error = root.value(QStringLiteral("error")).toString();
	state_->setToolTip(error);
}

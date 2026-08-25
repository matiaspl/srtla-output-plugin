#include "obs-srtla-dock.hpp"

#include "network-monitor.hpp"

#include <obs.h>
#include <obs-encoder.h>
#include <obs-frontend-api.h>

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
#include <QSpinBox>
#include <QTableWidget>
#include <QTableWidgetItem>
#include <QTimer>
#include <QVBoxLayout>
#include <QSettings>

#include <cstddef>
#include <cstdint>
#include <cstring>

extern "C" std::uint64_t srtla_output_link_id(const char *adapter_id);
extern "C" int srtla_output_set_link_enabled(struct obs_output *output, std::uint64_t link_id, bool enabled);
extern "C" int srtla_output_update_adapters(struct obs_output *output);
extern "C" std::size_t srtla_output_copy_stats_json(struct obs_output *output, char *buffer, std::size_t capacity);

namespace {

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
	maxBitrate_->setRange(500, 200000);
	maxBitrate_->setValue(100000);
	maxBitrate_->setSingleStep(50);
	maxBitrate_->setSuffix(tr(" kb/s"));
	state_ = new QLabel(tr("Idle"), this);
	metrics_ = new QLabel(tr("Capacity: -- | Used: -- | Active links: 0"), this);
	{
		QSettings settings;
		url_->setText(settings.value(QStringLiteral("obs-srtla/url"), url_->text()).toString());
		select_combo_data(encoderSource_, settings.value(QStringLiteral("obs-srtla/encoder_source"), QStringLiteral("streaming")).toString());
		select_combo_data(customVideoEncoder_, settings.value(QStringLiteral("obs-srtla/custom_video_encoder")).toString());
		select_combo_data(customAudioEncoder_, settings.value(QStringLiteral("obs-srtla/custom_audio_encoder")).toString());
		autoBitrate_->setChecked(settings.value(QStringLiteral("obs-srtla/auto_bitrate"), true).toBool());
		manualBitrate_->setValue(settings.value(QStringLiteral("obs-srtla/bitrate"), 1500).toInt());
		maxBitrate_->setValue(settings.value(QStringLiteral("obs-srtla/max_bitrate"), 100000).toInt());
	}
	controls->addRow(tr("State"), state_);
	controls->addRow(tr("SRTLA URL"), url_);
	controls->addRow(tr("Encoder"), encoderSource_);
	controls->addRow(tr("Custom video encoder"), customVideoEncoder_);
	controls->addRow(tr("Custom audio encoder"), customAudioEncoder_);
	controls->addRow(tr("Bitrate"), autoBitrate_);
	controls->addRow(tr("Manual"), manualBitrate_);
	controls->addRow(tr("Maximum"), maxBitrate_);
	layout->addLayout(controls);
	layout->addWidget(metrics_);
	layout->addWidget(startStop_);

	links_ = new QTableWidget(this);
	links_->setColumnCount(8);
	links_->setHorizontalHeaderLabels({tr("Use"), tr("Adapter / IP"), tr("State"), tr("Quality"), tr("RTT"), tr("Loss"), tr("Used"), tr("Capacity")});
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
		QSettings settings;
		settings.setValue(QStringLiteral("obs-srtla/links/") + QString::fromStdString(key), selected_[key]);
	});
	layout->addWidget(links_, 1);

	connect(startStop_, &QPushButton::clicked, this, &SrtlaDock::toggleOutput);
	connect(autoBitrate_, &QCheckBox::toggled, manualBitrate_, &QSpinBox::setDisabled);
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

void SrtlaDock::stopOutput()
{
	if (!running_)
		return;
	running_ = false;
	if (output_ && obs_output_active(output_))
		obs_output_stop(output_);
	startStop_->setText(tr("Start"));
	state_->setText(tr("Idle"));
}

void SrtlaDock::refreshAdapters()
{
	const auto adapters = NetworkMonitor().enumerate();
	QSettings settings;
	QSignalBlocker blocker(links_);
	links_->setRowCount(static_cast<int>(adapters.size()));
	for (int row = 0; row < links_->rowCount(); ++row) {
		const auto &adapter = adapters[static_cast<size_t>(row)];
		const auto persisted = settings.value(QStringLiteral("obs-srtla/links/") + QString::fromStdString(adapter.id));
		const bool enabled = persisted.isValid() ? persisted.toBool() : adapter.enabled;
		selected_[adapter.id] = enabled;
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
	}
	if (output_ && running_)
		srtla_output_update_adapters(output_);
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

	obs_data_set_string(settings_, "url", url_->text().toUtf8().constData());
	{
		QSettings settings;
		settings.setValue(QStringLiteral("obs-srtla/url"), url_->text());
		settings.setValue(QStringLiteral("obs-srtla/encoder_source"), encoder_source);
		settings.setValue(QStringLiteral("obs-srtla/custom_video_encoder"), customVideoEncoder_->currentData().toString());
		settings.setValue(QStringLiteral("obs-srtla/custom_audio_encoder"), customAudioEncoder_->currentData().toString());
		settings.setValue(QStringLiteral("obs-srtla/auto_bitrate"), autoBitrate_->isChecked());
		settings.setValue(QStringLiteral("obs-srtla/bitrate"), manualBitrate_->value());
		settings.setValue(QStringLiteral("obs-srtla/max_bitrate"), maxBitrate_->value());
	}
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
	obs_data_set_double(settings_, "safety_margin", 0.80);
	obs_data_set_int(settings_, "latency_ms", 2000);
	obs_data_set_int(settings_, "pbkeylen", 16);
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
		state_->setText(tr("Error: output start failed"));
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
	QByteArray buffer(static_cast<int>(required), '\0');
	srtla_output_copy_stats_json(output_, buffer.data(), required);
	QJsonParseError parse_error{};
	const auto document = QJsonDocument::fromJson(buffer, &parse_error);
	if (parse_error.error != QJsonParseError::NoError || !document.isObject())
		return;
	const auto root = document.object();
	const auto links = root.value(QStringLiteral("links")).toArray();
	int active = 0;
	quint64 capacity = 0;
	quint64 used = 0;
	for (const auto &value : links) {
		const auto link = value.toObject();
		if (link.value(QStringLiteral("payload_eligible")).toBool()) {
			++active;
			capacity += link.value(QStringLiteral("target_bps")).toInteger();
		}
		used += link.value(QStringLiteral("used_bps")).toInteger();
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
			links_->item(row, 7)->setText(QString::number(link.value(QStringLiteral("target_bps")).toInteger() / 1000) + QStringLiteral(" kb/s"));
			identity->setToolTip(tr("CC: %1 | stall events: %2 | NAK: %3 | scheduler eligibility: %4 | reason: %5")
				.arg(link.value(QStringLiteral("cc_state")).toString())
				.arg(link.value(QStringLiteral("stall_gate_events")).toInteger())
				.arg(link.value(QStringLiteral("nak_count")).toInteger())
				.arg(link.value(QStringLiteral("payload_eligible")).toBool() ? tr("eligible") : tr("excluded"))
				.arg(link.value(QStringLiteral("exclusion_reason")).toString()));
			break;
		}
	}
	metrics_->setText(tr("Capacity: %1 kb/s | Used: %2 kb/s | Active links: %3")
			.arg(capacity / 1000).arg(used / 1000).arg(active)
			+ tr(" | Video: %1 kb/s | Recommended: %2 kb/s")
				.arg(root.value(QStringLiteral("current_video_bps")).toInteger() / 1000)
				.arg(root.value(QStringLiteral("recommended_video_bps")).toInteger() / 1000));
	const auto state = root.value(QStringLiteral("state")).toString(active > 0 ? tr("Live") : tr("Waiting for network"));
	state_->setText(state);
	const auto error = root.value(QStringLiteral("error")).toString();
	state_->setToolTip(error);
}

#include "srtla-dock.hpp"
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
#include <QFrame>
#include <QGridLayout>
#include <QHBoxLayout>
#include <QHeaderView>
#include <QFrame>
#include <QJsonArray>
#include <QJsonDocument>
#include <QJsonObject>
#include <QLabel>
#include <QLineEdit>
#include <QPainter>
#include <QPainterPath>
#include <QSignalBlocker>
#include <QPushButton>
#include <QResizeEvent>
#include <QScrollBar>
#include <QSizePolicy>
#include <QToolButton>
#include <QUrl>
#include <QUrlQuery>
#include <QSpinBox>
#include <QTableWidget>
#include <QTableWidgetItem>
#include <QTimer>
#include <QVBoxLayout>
#include <QGridLayout>

#include <cstddef>
#include <cstdint>
#include <cstring>
#include <algorithm>
#include <cmath>
#include <limits>
#include <tuple>
#include <vector>

extern "C" std::uint64_t srtla_output_link_id(const char *adapter_id);
extern "C" int srtla_output_set_link_enabled(struct obs_output *output, std::uint64_t link_id, bool enabled);
extern "C" int srtla_output_set_bitrate_control(struct obs_output *output, bool automatic,
	                                            int manual_bitrate_kbps);
extern "C" int srtla_output_set_max_bitrate(struct obs_output *output, int max_bitrate_kbps);
extern "C" int srtla_output_update_adapters(struct obs_output *output);
extern "C" std::size_t srtla_output_copy_stats_json(struct obs_output *output, char *buffer, std::size_t capacity);
void srtla_websocket_emit_status_json(const char *json);

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
		blog(LOG_WARNING, "[srtla-output] Failed to save dock settings");
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

class SparklineWidget final : public QWidget {
public:
	explicit SparklineWidget(QWidget *parent = nullptr) : QWidget(parent)
	{
		setMinimumSize(48, 18);
		setMaximumHeight(24);
		setSizePolicy(QSizePolicy::Expanding, QSizePolicy::Fixed);
		setAttribute(Qt::WA_TransparentForMouseEvents);
	}

	void setHistory(const std::deque<double> &values, const std::deque<double> &reference = {}, bool stepped = false,
			 int windowSeconds = 60, QColor valueColor = QColor(130, 185, 255))
	{
		values_ = values;
		reference_ = reference;
		stepped_ = stepped;
		windowSeconds_ = std::max(1, windowSeconds);
		valueColor_ = std::move(valueColor);
		update();
	}

protected:
	void paintEvent(QPaintEvent *) override
	{
		QPainter painter(this);
		painter.setRenderHint(QPainter::Antialiasing);
		const QRectF area = QRectF(rect()).adjusted(2.0, 2.0, -2.0, -2.0);
		if (area.width() < 2.0 || area.height() < 2.0)
			return;
		if (values_.size() < 2) {
			painter.setPen(palette().color(QPalette::WindowText));
			painter.drawText(area, Qt::AlignCenter, QStringLiteral("—"));
			return;
		}

		double minimum = std::numeric_limits<double>::infinity();
		double maximum = -std::numeric_limits<double>::infinity();
		const auto inspect = [&](const std::deque<double> &series) {
			for (const double value : series) {
				if (!std::isfinite(value))
					continue;
				minimum = std::min(minimum, value);
				maximum = std::max(maximum, value);
			}
		};
		inspect(values_);
		inspect(reference_);
		if (!std::isfinite(minimum) || !std::isfinite(maximum))
			return;
		if (maximum - minimum < 0.001) {
			const double padding = std::max(1.0, std::abs(maximum) * 0.08);
			minimum -= padding;
			maximum += padding;
		} else {
			const double padding = (maximum - minimum) * 0.10;
			minimum -= padding;
			maximum += padding;
		}

		painter.setPen(QPen(palette().color(QPalette::Mid), 1.0));
		painter.drawLine(QPointF(area.left(), area.bottom()), QPointF(area.right(), area.bottom()));
		const auto drawSeries = [&](const std::deque<double> &series, const QColor &color, Qt::PenStyle style) {
			if (series.size() < 2)
				return;
			QPainterPath path;
			bool started = false;
			QPointF previous;
			for (size_t index = 0; index < series.size(); ++index) {
				const double value = series[index];
				if (!std::isfinite(value)) {
					started = false;
					continue;
				}
				const double elapsed = static_cast<double>(windowSeconds_ - static_cast<int>(series.size()) + static_cast<int>(index));
				const double x = area.left() + elapsed * area.width() / std::max(1, windowSeconds_ - 1);
				const double y = area.bottom() - (value - minimum) / (maximum - minimum) * area.height();
				const QPointF point(x, y);
				if (!started) {
					path.moveTo(point);
					started = true;
				} else if (stepped_) {
					path.lineTo(point.x(), previous.y());
					path.lineTo(point);
				} else {
					path.lineTo(point);
				}
				previous = point;
			}
			// Keep a narrow base-colored edge around each series so it remains
			// distinct over both the normal cell background and the row selection.
			painter.setPen(QPen(palette().color(QPalette::Window), 3.2, style, Qt::RoundCap, Qt::RoundJoin));
			painter.drawPath(path);
			painter.setPen(QPen(color, 1.5, style, Qt::RoundCap, Qt::RoundJoin));
			painter.drawPath(path);
		};
		drawSeries(reference_, QColor(225, 173, 82), Qt::DashLine);
		drawSeries(values_, valueColor_, Qt::SolidLine);
	}

private:
	std::deque<double> values_;
	std::deque<double> reference_;
	bool stepped_ = false;
	int windowSeconds_ = 60;
	QColor valueColor_{130, 185, 255};
};

class HistoryChartWidget final : public QWidget {
public:
	struct Series {
		QString name;
		std::deque<double> values;
		QColor color;
		Qt::PenStyle style = Qt::SolidLine;
	};

	explicit HistoryChartWidget(QWidget *parent = nullptr) : QWidget(parent)
	{
		setMinimumHeight(94);
		setSizePolicy(QSizePolicy::Expanding, QSizePolicy::Expanding);
	}

	void setSeries(std::vector<Series> series, int windowSeconds)
	{
		series_ = std::move(series);
		windowSeconds_ = windowSeconds;
		update();
	}

protected:
	void paintEvent(QPaintEvent *) override
	{
		QPainter painter(this);
		painter.setRenderHint(QPainter::Antialiasing);
		const QRectF bounds = QRectF(rect()).adjusted(2.0, 2.0, -2.0, -2.0);
		if (bounds.width() < 60.0 || bounds.height() < 35.0)
			return;

		size_t sampleCount = 0;
		for (const auto &series : series_)
			sampleCount = std::max(sampleCount, std::min(series.values.size(), static_cast<size_t>(windowSeconds_)));
		if (sampleCount < 2) {
			painter.setPen(palette().color(QPalette::WindowText));
			painter.drawText(bounds, Qt::AlignCenter, tr("No history yet"));
			return;
		}

		double minimum = std::numeric_limits<double>::infinity();
		double maximum = -std::numeric_limits<double>::infinity();
		for (const auto &series : series_) {
			const size_t count = std::min(series.values.size(), static_cast<size_t>(windowSeconds_));
			for (size_t index = series.values.size() - count; index < series.values.size(); ++index) {
				const double value = series.values[index];
				if (!std::isfinite(value))
					continue;
				minimum = std::min(minimum, value);
				maximum = std::max(maximum, value);
			}
		}
		if (!std::isfinite(minimum) || !std::isfinite(maximum)) {
			painter.setPen(palette().color(QPalette::Mid));
			painter.drawText(bounds, Qt::AlignCenter, tr("Waiting for telemetry"));
			return;
		}
		if (maximum - minimum < 0.001) {
			const double padding = std::max(1.0, std::abs(maximum) * 0.08);
			minimum = std::max(0.0, minimum - padding);
			maximum += padding;
		} else {
			const double padding = (maximum - minimum) * 0.10;
			minimum = std::max(0.0, minimum - padding);
			maximum += padding;
		}

		const QRectF plot = bounds.adjusted(34.0, 5.0, -5.0, -20.0);
		QColor grid = palette().color(QPalette::Midlight);
		grid.setAlpha(130);
		const QColor text = palette().color(QPalette::WindowText);
		for (int tick = 0; tick < 3; ++tick) {
			const double fraction = tick / 2.0;
			const double value = minimum + fraction * (maximum - minimum);
			const double y = plot.bottom() - fraction * plot.height();
			painter.setPen(QPen(grid, 1.0));
			painter.drawLine(QPointF(plot.left(), y), QPointF(plot.right(), y));
			painter.setPen(text);
			painter.drawText(QRectF(bounds.left(), y - 8.0, 29.0, 16.0), Qt::AlignRight | Qt::AlignVCenter,
				QString::number(value, 'f', maximum < 10.0 ? 1 : 0));
		}
		const std::array<QString, 3> times = {
			QStringLiteral("−%1s").arg(windowSeconds_),
			QStringLiteral("−%1s").arg(windowSeconds_ / 2), QStringLiteral("Now")};
		for (size_t tick = 0; tick < times.size(); ++tick) {
			const double x = plot.left() + tick * plot.width() / 2.0;
			painter.setPen(text);
			painter.drawText(QRectF(x - 35.0, plot.bottom() + 2.0, 70.0, 16.0), Qt::AlignHCenter | Qt::AlignVCenter, times[tick]);
		}

		for (const auto &series : series_) {
			const size_t count = std::min(series.values.size(), static_cast<size_t>(windowSeconds_));
			if (count < 2)
				continue;
			const size_t start = series.values.size() - count;
			QPainterPath path;
			bool started = false;
			for (size_t index = 0; index < count; ++index) {
				const double value = series.values[start + index];
				if (!std::isfinite(value)) {
					started = false;
					continue;
				}
				const double elapsedOffset = static_cast<double>(windowSeconds_ - count + index);
				const double x = plot.left() + elapsedOffset * plot.width() /
					std::max(1, windowSeconds_ - 1);
				const double y = plot.bottom() - (value - minimum) * plot.height() / (maximum - minimum);
				if (!started) {
					path.moveTo(x, y);
					started = true;
				} else {
					path.lineTo(x, y);
				}
			}
			painter.setPen(QPen(series.color, 1.7, series.style, Qt::RoundCap, Qt::RoundJoin));
			painter.drawPath(path);
		}
	}

private:
	std::vector<Series> series_;
	int windowSeconds_ = 60;
};

namespace {

void append_sample(std::deque<double> &history, double value)
{
	history.push_back(value);
	while (history.size() > 900)
		history.pop_front();
}

std::deque<double> history_tail(const std::deque<double> &history, int seconds)
{
	const auto count = std::min(history.size(), static_cast<size_t>(seconds));
	return {history.end() - static_cast<std::ptrdiff_t>(count), history.end()};
}

QColor link_sparkline_color(int column)
{
	switch (column) {
	case 3: return QColor(111, 199, 164);  // NAK score
	case 4: return QColor(130, 185, 255);  // RTT
	case 5: return QColor(199, 146, 234);  // NAK rate
	case 6: return QColor(184, 196, 216);  // Offered
	case 7: return QColor(111, 199, 164);  // Delivered
	case 8: return QColor(225, 173, 82);   // CC target
	default: return QColor(130, 185, 255);
	}
}

void set_telemetry_cell(QTableWidget *table, int row, int column, const QString &value,
			const std::deque<double> &history, const std::deque<double> &reference = {}, int windowSeconds = 60)
{
	auto *cell = table->cellWidget(row, column);
	if (!cell) {
		cell = new QWidget(table);
		auto *cellLayout = new QVBoxLayout(cell);
		cellLayout->setContentsMargins(2, 1, 2, 1);
		cellLayout->setSpacing(0);
		auto *text = new QLabel(cell);
		text->setObjectName(QStringLiteral("telemetryValue"));
		text->setAlignment(Qt::AlignCenter);
		cellLayout->addWidget(text);
		auto *plot = new SparklineWidget(cell);
		plot->setObjectName(QStringLiteral("telemetryHistory"));
		plot->setToolTip(QObject::tr("Recent trend; independently scaled. Select a link for full history."));
		cellLayout->addWidget(plot);
		table->setCellWidget(row, column, cell);
	}
	if (auto *text = cell->findChild<QLabel *>(QStringLiteral("telemetryValue")))
		text->setText(value);
	if (auto *plot = cell->findChild<QWidget *>(QStringLiteral("telemetryHistory")))
		static_cast<SparklineWidget *>(plot)->setHistory(history_tail(history, windowSeconds), history_tail(reference, windowSeconds), false,
										 windowSeconds, link_sparkline_color(column));
}

QString abr_sparkline_label(const QString &state)
{
	if (state == QStringLiteral("Increasing")) return QStringLiteral("Increasing");
	if (state == QStringLiteral("Light congestion")) return QStringLiteral("Light");
	if (state == QStringLiteral("Heavy congestion")) return QStringLiteral("Heavy");
	if (state == QStringLiteral("Severe congestion")) return QStringLiteral("Severe");
	if (state == QStringLiteral("Disconnected")) return QStringLiteral("Disconnected");
	if (state == QStringLiteral("Waiting for SRT feedback")) return QStringLiteral("Waiting");
	return QStringLiteral("Stable");
}

double abr_sparkline_level(const QString &state)
{
	if (state == QStringLiteral("Increasing")) return 1.0;
	if (state == QStringLiteral("Light congestion")) return 2.0;
	if (state == QStringLiteral("Heavy congestion")) return 3.0;
	if (state == QStringLiteral("Severe congestion")) return 4.0;
	if (state == QStringLiteral("Disconnected")) return -1.0;
	if (state == QStringLiteral("Waiting for SRT feedback")) return 0.0;
	return 0.5;
}

} // namespace

SrtlaDock::SrtlaDock(QWidget *parent) : QWidget(parent)
{
	setObjectName(QStringLiteral("SrtlaOutputDock"));
	auto *layout = new QVBoxLayout(this);
	layout->setContentsMargins(8, 8, 8, 8);
	layout->setSpacing(6);
	startStop_ = new QPushButton(tr("Start"), this);
	state_ = new QLabel(tr("Idle"), this);
	state_->setStyleSheet(QStringLiteral("font-weight: 600;"));

	auto *overview = new QFrame(this);
	overview->setFrameShape(QFrame::StyledPanel);
	auto *overviewBox = new QVBoxLayout(overview);
	overviewBox->setContentsMargins(8, 6, 8, 6);
	overviewBox->setSpacing(5);
	auto *statusBar = new QHBoxLayout();
	statusBar->setContentsMargins(0, 0, 0, 0);
	statusBar->addWidget(state_);
	overviewTitle_ = new QLabel(tr("Output overview · last 60 seconds"), overview);
	statusBar->addStretch(1);
	statusBar->addWidget(overviewTitle_);
	statusBar->addStretch(1);
	statusBar->addWidget(startStop_);
	overviewBox->addLayout(statusBar);
	overviewLayout_ = new QGridLayout();
	overviewLayout_->setContentsMargins(0, 0, 0, 0);
	overviewLayout_->setHorizontalSpacing(14);
	overviewLayout_->setVerticalSpacing(5);
	const std::array<QString, 6> metricNames = {
		tr("ABR"), tr("RTT / latency"), tr("SRT queue"),
		tr("Video"), tr("Target"), tr("Active links")};
	for (int index = 0; index < static_cast<int>(metricNames.size()); ++index) {
		auto *tile = new QWidget(overview);
		metricTiles_[static_cast<size_t>(index)] = tile;
		auto *tileLayout = new QVBoxLayout(tile);
		tileLayout->setContentsMargins(0, 0, 0, 0);
		tileLayout->setSpacing(1);
		auto *title = new QLabel(metricNames[static_cast<size_t>(index)], tile);
		QFont titleFont = title->font();
		titleFont.setPointSize(std::max(8, titleFont.pointSize() - 1));
		title->setFont(titleFont);
		metricValues_[static_cast<size_t>(index)] = new QLabel(QStringLiteral("--"), tile);
		QFont valueFont = metricValues_[static_cast<size_t>(index)]->font();
		valueFont.setPointSize(valueFont.pointSize() + 2);
		valueFont.setWeight(QFont::DemiBold);
		metricValues_[static_cast<size_t>(index)]->setFont(valueFont);
		metricValues_[static_cast<size_t>(index)]->setMinimumWidth(55);
		metricSparklines_[static_cast<size_t>(index)] = new SparklineWidget(tile);
		metricSparklines_[static_cast<size_t>(index)]->setToolTip(tr("Recent history; updates once per second"));
		tileLayout->addWidget(title);
		tileLayout->addWidget(metricValues_[static_cast<size_t>(index)]);
		tileLayout->addWidget(metricSparklines_[static_cast<size_t>(index)]);
	}
	overviewBox->addLayout(overviewLayout_);
	arrangeOverview();
	layout->addWidget(overview);

	auto *bitrateRow = new QHBoxLayout();
	bitrateRow->setContentsMargins(0, 0, 0, 0);
	autoBitrate_ = new QCheckBox(tr("Auto bitrate"), this);
	autoBitrate_->setChecked(true);
	auto *videoBitrateLabel = new QLabel(tr("Video bitrate"), this);
	manualBitrate_ = new QSpinBox(this);
	manualBitrate_->setRange(500, 100000);
	manualBitrate_->setValue(1500);
	manualBitrate_->setSuffix(tr(" kb/s"));
	manualBitrate_->setEnabled(false);
	videoBitrateLabel->setBuddy(manualBitrate_);
	maxBitrate_ = new QSpinBox(this);
	maxBitrate_->setRange(500, 30000);
	maxBitrate_->setValue(6000);
	maxBitrate_->setSingleStep(50);
	maxBitrate_->setSuffix(tr(" kb/s max"));
	bitrateRow->addWidget(autoBitrate_);
	bitrateRow->addWidget(videoBitrateLabel);
	bitrateRow->addWidget(manualBitrate_, 1);
	bitrateRow->addWidget(maxBitrate_, 1);
	layout->addLayout(bitrateRow);

	auto *settingsHeader = new QHBoxLayout();
	settingsHeader->setContentsMargins(0, 0, 0, 0);
	auto *settingsToggle = new QToolButton(this);
	settingsToggle->setText(tr("Connection && encoding settings"));
	settingsToggle->setToolButtonStyle(Qt::ToolButtonTextBesideIcon);
	settingsToggle->setArrowType(Qt::RightArrow);
	settingsToggle->setCheckable(true);
	settingsToggle->setChecked(false);
	settingsSummary_ = new QLabel(this);
	settingsSummary_->setTextInteractionFlags(Qt::TextSelectableByMouse);
	settingsHeader->addWidget(settingsToggle);
	settingsHeader->addWidget(settingsSummary_, 1);
	layout->addLayout(settingsHeader);

	connectionSettings_ = new QWidget(this);
	connectionSettings_->setVisible(false);
	auto *settingsGrid = new QGridLayout(connectionSettings_);
	settingsGrid->setContentsMargins(0, 0, 0, 2);
	settingsGrid->setHorizontalSpacing(8);
	settingsGrid->setVerticalSpacing(4);
	url_ = new QLineEdit(QStringLiteral("srtla://receiver.example:5000"), connectionSettings_);
	streamId_ = new QLineEdit(connectionSettings_);
	passphrase_ = new QLineEdit(connectionSettings_);
	passphrase_->setEchoMode(QLineEdit::Password);
	secret_warning_ = new QLabel(tr("Passphrase is stored unencrypted in the OBS profile."), connectionSettings_);
	secret_warning_->setWordWrap(true);
	secret_warning_->setStyleSheet(QStringLiteral("color: #b36b00;"));
	encoderSource_ = new QComboBox(connectionSettings_);
	encoderSource_->addItem(tr("Streaming"), QStringLiteral("streaming"));
	encoderSource_->addItem(tr("Recording"), QStringLiteral("recording"));
	encoderSource_->addItem(tr("Custom"), QStringLiteral("custom"));
	customVideoEncoder_ = new QComboBox(connectionSettings_);
	add_encoder_choices(customVideoEncoder_, OBS_ENCODER_VIDEO);
	customAudioEncoder_ = new QComboBox(connectionSettings_);
	add_encoder_choices(customAudioEncoder_, OBS_ENCODER_AUDIO);
	loadProfile();
	connect(passphrase_, &QLineEdit::textChanged, this, [this] {
		if (!secret_error_)
			return;
		secret_error_ = false;
		secret_warning_->setText(tr("Passphrase is stored unencrypted in the OBS profile."));
	});
	url_->setPlaceholderText(tr("srtla://host:port"));
	streamId_->setPlaceholderText(tr("Optional"));
	passphrase_->setPlaceholderText(tr("Optional"));
	settingsGrid->addWidget(new QLabel(tr("SRTLA URL"), connectionSettings_), 0, 0);
	settingsGrid->addWidget(url_, 0, 1, 1, 3);
	settingsGrid->addWidget(new QLabel(tr("Stream ID"), connectionSettings_), 1, 0);
	settingsGrid->addWidget(streamId_, 1, 1);
	settingsGrid->addWidget(new QLabel(tr("Passphrase"), connectionSettings_), 1, 2);
	settingsGrid->addWidget(passphrase_, 1, 3);
	settingsGrid->addWidget(secret_warning_, 2, 1, 1, 3);
	settingsGrid->addWidget(new QLabel(tr("Encoder source"), connectionSettings_), 3, 0);
	settingsGrid->addWidget(encoderSource_, 3, 1);
	settingsGrid->addWidget(new QLabel(tr("Custom video encoder"), connectionSettings_), 3, 2);
	settingsGrid->addWidget(customVideoEncoder_, 3, 3);
	settingsGrid->addWidget(new QLabel(tr("Custom audio encoder"), connectionSettings_), 4, 2);
	settingsGrid->addWidget(customAudioEncoder_, 4, 3);
	layout->addWidget(connectionSettings_);
	auto updateSettingsSummary = [this] {
		const auto endpoint = url_->text().isEmpty() ? tr("No receiver URL") : url_->text();
		auto encoder = encoderSource_->currentText();
		if (encoderSource_->currentData().toString() == QStringLiteral("custom"))
			encoder += QStringLiteral(" · %1 / %2").arg(customVideoEncoder_->currentText(), customAudioEncoder_->currentText());
		settingsSummary_->setText(QStringLiteral("%1  ·  %2").arg(endpoint, encoder));
	};
	connect(settingsToggle, &QToolButton::toggled, this, [this, settingsToggle](bool expanded) {
		connectionSettings_->setVisible(expanded);
		settingsToggle->setArrowType(expanded ? Qt::DownArrow : Qt::RightArrow);
	});
	connect(url_, &QLineEdit::textChanged, this, [updateSettingsSummary] { updateSettingsSummary(); });
	connect(encoderSource_, QOverload<int>::of(&QComboBox::currentIndexChanged), this,
		[updateSettingsSummary](int) { updateSettingsSummary(); });
	connect(customVideoEncoder_, QOverload<int>::of(&QComboBox::currentIndexChanged), this,
		[updateSettingsSummary](int) { updateSettingsSummary(); });
	connect(customAudioEncoder_, QOverload<int>::of(&QComboBox::currentIndexChanged), this,
		[updateSettingsSummary](int) { updateSettingsSummary(); });
	updateSettingsSummary();

	auto *adapterHeader = new QHBoxLayout();
	adapterHeader->setContentsMargins(0, 0, 0, 0);
	auto *adapterTitle = new QLabel(tr("Network adapters"), this);
	QFont sectionFont = adapterTitle->font();
	sectionFont.setWeight(QFont::DemiBold);
	adapterTitle->setFont(sectionFont);
	adapterCount_ = new QLabel(this);
	showAllAdapters_ = new QCheckBox(tr("Show all adapters"), this);
	showAllAdapters_->setToolTip(tr("Include unchecked adapters in the list"));
	adapterHeader->addWidget(adapterTitle);
	adapterHeader->addWidget(adapterCount_);
	adapterHeader->addStretch(1);
	adapterHeader->addWidget(showAllAdapters_);
	layout->addLayout(adapterHeader);
	connect(showAllAdapters_, &QCheckBox::toggled, this, [this] { applyAdapterVisibility(); });
	trendHint_ = new QLabel(tr("Inline trends · same window"), this);
	trendHint_->setAlignment(Qt::AlignRight);
	layout->addWidget(trendHint_);

	links_ = new QTableWidget(this);
	links_->setColumnCount(9);
	links_->setHorizontalHeaderLabels({tr("Use"), tr("Adapter / IP"), tr("State"), tr("NAK score\n%"), tr("RTT\nms"), tr("NAK rate\n%"), tr("Offered\nMb/s"), tr("Delivered\nMb/s"), tr("CC target\nMb/s")});
	links_->horizontalHeaderItem(3)->setToolTip(tr("Network path quality score used by the link scheduler."));
	links_->horizontalHeaderItem(5)->setToolTip(tr("SRT retransmission-request rate. Requests may be recovered; this is not final packet loss."));
	links_->horizontalHeader()->setSectionResizeMode(0, QHeaderView::ResizeToContents);
	links_->horizontalHeader()->setSectionResizeMode(1, QHeaderView::Stretch);
	links_->horizontalHeader()->setSectionResizeMode(2, QHeaderView::ResizeToContents);
	for (int column = 3; column < links_->columnCount(); ++column)
		links_->horizontalHeader()->setSectionResizeMode(column, QHeaderView::ResizeToContents);
	links_->verticalHeader()->setDefaultSectionSize(43);
	links_->verticalHeader()->setMinimumSectionSize(40);
	links_->setAlternatingRowColors(true);
	links_->setEditTriggers(QAbstractItemView::NoEditTriggers);
	links_->setWordWrap(false);
	links_->setTextElideMode(Qt::ElideMiddle);
	links_->setSelectionBehavior(QAbstractItemView::SelectRows);
	links_->setSelectionMode(QAbstractItemView::SingleSelection);
	links_->setSizePolicy(QSizePolicy::Expanding, QSizePolicy::Fixed);
	links_->setHorizontalScrollMode(QAbstractItemView::ScrollPerPixel);
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
		applyAdapterVisibility();
	});
	connect(links_, &QTableWidget::itemSelectionChanged, this, [this] {
		const int row = links_->currentRow();
		const auto *identity = row >= 0 ? links_->item(row, 1) : nullptr;
		if (identity)
			selectedLinkKey_ = identity->data(Qt::UserRole).toString().toStdString();
		updateHistoryPanel();
	});
	layout->addWidget(links_);

	auto *historyPanel = new QFrame(this);
	historyPanel->setFrameShape(QFrame::StyledPanel);
	historyPanel_ = historyPanel;
	auto *historyLayout = new QVBoxLayout(historyPanel);
	historyLayout->setContentsMargins(8, 6, 8, 6);
	historyLayout->setSpacing(4);
	auto *historyHeader = new QHBoxLayout();
	historyHeader->setContentsMargins(0, 0, 0, 0);
	historyTitle_ = new QLabel(tr("Link history"), historyPanel);
	QFont historyTitleFont = historyTitle_->font();
	historyTitleFont.setWeight(QFont::DemiBold);
	historyTitle_->setFont(historyTitleFont);
	historyHeader->addWidget(historyTitle_);
	historyHeader->addStretch(1);
	historyRange_ = new QComboBox(historyPanel);
	historyRange_->addItem(tr("1 min"), 60);
	historyRange_->addItem(tr("5 min"), 300);
	historyRange_->addItem(tr("15 min"), 900);
	historyRange_->setToolTip(tr("History window for the overview and selected adapter"));
	historyHeader->addWidget(historyRange_);
	historyCollapse_ = new QToolButton(historyPanel);
	historyCollapse_->setText(tr("Hide history"));
	historyCollapse_->setCheckable(true);
	historyCollapse_->setChecked(true);
	historyHeader->addWidget(historyCollapse_);
	historyLayout->addLayout(historyHeader);
	auto *historyBody = new QWidget(historyPanel);
	auto *historyBodyLayout = new QVBoxLayout(historyBody);
	historyBodyLayout->setContentsMargins(0, 0, 0, 0);
	historyBodyLayout->setSpacing(4);
	auto *legend = new QHBoxLayout();
	legend->setContentsMargins(0, 0, 0, 0);
	const auto addLegendItem = [legend, historyPanel](const QString &name, const QColor &color, Qt::PenStyle style) {
		auto *swatch = new QLabel(style == Qt::DashLine ? QStringLiteral("┄┄") : QStringLiteral("━━"), historyPanel);
		swatch->setStyleSheet(QStringLiteral("color: %1; font-weight: 600;").arg(color.name()));
		legend->addWidget(swatch);
		legend->addWidget(new QLabel(name, historyPanel));
	};
	addLegendItem(tr("Offered"), QColor(184, 196, 216), Qt::SolidLine);
	addLegendItem(tr("Delivered"), QColor(111, 199, 164), Qt::SolidLine);
	addLegendItem(tr("CC target"), QColor(225, 173, 82), Qt::DashLine);
	legend->addWidget(new QLabel(tr("Mb/s · shared scale"), historyPanel));
	legend->addStretch(1);
	historyBodyLayout->addLayout(legend);
	auto *chartGrid = new QGridLayout();
	chartGrid->setContentsMargins(0, 0, 0, 0);
	chartGrid->setHorizontalSpacing(12);
	chartGrid->setVerticalSpacing(4);
	const std::array<QString, 3> chartTitles = {tr("Offered, delivered & congestion target"), tr("RTT · ms"), tr("NAK rate · %")};
	for (size_t index = 0; index < historyCharts_.size(); ++index) {
		auto *container = new QWidget(historyBody);
		auto *chartLayout = new QVBoxLayout(container);
		chartLayout->setContentsMargins(0, 0, 0, 0);
		chartLayout->setSpacing(1);
		auto *title = new QLabel(chartTitles[index], container);
		chartLayout->addWidget(title);
		historyCharts_[index] = new HistoryChartWidget(container);
		historyCharts_[index]->setMinimumHeight(index == 0 ? 124 : 96);
		chartLayout->addWidget(historyCharts_[index], 1);
		chartGrid->addWidget(container, index == 0 ? 0 : 1, index == 0 ? 0 : static_cast<int>(index - 1),
			index == 0 ? 1 : 1, index == 0 ? 2 : 1);
	}
	chartGrid->setColumnStretch(0, 1);
	chartGrid->setColumnStretch(1, 1);
	historyBodyLayout->addLayout(chartGrid, 1);
	historyNow_ = new QLabel(tr("No history yet · start output to collect link telemetry"), historyBody);
	historyBodyLayout->addWidget(historyNow_);
	historyLayout->addWidget(historyBody, 1);
	connect(historyRange_, QOverload<int>::of(&QComboBox::currentIndexChanged), this, [this](int index) {
		historyWindowSeconds_ = historyRange_->itemData(index).toInt();
		updateOverviewSparklines();
		updateHistoryPanel();
	});
	connect(historyCollapse_, &QToolButton::toggled, this, [this, historyBody](bool expanded) {
		historyBody->setVisible(expanded);
		historyCollapse_->setText(expanded ? tr("Hide history") : tr("Show history"));
	});
	layout->addWidget(historyPanel_);
	layout->addStretch(1);
	updateHistoryPanel();

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

QByteArray SrtlaDock::websocketStatusJson()
{
	if (!running_ || !output_) {
		QJsonObject status;
		status.insert(QStringLiteral("running"), false);
		const auto dockState = state_ ? state_->text() : QString();
		const bool failedStart = dockState.startsWith(QStringLiteral("Error:"));
		status.insert(QStringLiteral("state"), failedStart ? QStringLiteral("Error") : QStringLiteral("Idle"));
		if (failedStart)
			status.insert(QStringLiteral("error"), dockState.mid(6).trimmed());
		status.insert(QStringLiteral("automatic_bitrate"), autoBitrate_->isChecked());
		status.insert(QStringLiteral("manual_bitrate_kbps"), manualBitrate_->value());
		status.insert(QStringLiteral("max_bitrate_kbps"), maxBitrate_->value());
		QJsonArray links;
		for (int row = 0; row < links_->rowCount(); ++row) {
			const auto *identity = links_->item(row, 1);
			if (!identity)
				continue;
			const auto adapterId = identity->data(Qt::UserRole).toString();
			QJsonObject link;
			link.insert(QStringLiteral("id"), QString::number(static_cast<qulonglong>(
				srtla_output_link_id(adapterId.toUtf8().constData()))));
			link.insert(QStringLiteral("label"), identity->text());
			link.insert(QStringLiteral("admin_enabled"), selected_[adapterId.toStdString()]);
			link.insert(QStringLiteral("connected"), false);
			link.insert(QStringLiteral("state"), QStringLiteral("Idle"));
			links.append(link);
		}
		status.insert(QStringLiteral("links"), links);
		return QJsonDocument(status).toJson(QJsonDocument::Compact);
	}
	const auto required = srtla_output_copy_stats_json(output_, nullptr, 0);
	if (required == 0)
		return {};
	QByteArray buffer(static_cast<qsizetype>(required), '\0');
	const auto actual = srtla_output_copy_stats_json(output_, buffer.data(), required);
	if (actual == 0 || actual > required)
		return {};
	QJsonParseError parseError{};
	const auto document = srtla::parse_stats_json(buffer, actual, &parseError);
	if (parseError.error != QJsonParseError::NoError || !document.isObject())
		return {};
	QByteArray json(buffer.constData(), static_cast<qsizetype>(actual - 1));
	const auto controlSettings = QStringLiteral("\"automatic_bitrate\":%1,\"manual_bitrate_kbps\":%2,\"max_bitrate_kbps\":%3,")
		.arg(autoBitrate_->isChecked() ? QStringLiteral("true") : QStringLiteral("false"))
		.arg(manualBitrate_->value())
		.arg(maxBitrate_->value()).toUtf8();
	json.insert(1, controlSettings);
	return json;
}

bool SrtlaDock::websocketSetOutputActive(bool active, QString &error)
{
	if (running_ == active)
		return true;
	toggleOutput();
	if (running_ != active) {
		error = state_ ? state_->text() : tr("Unable to change SRTLA output state.");
		return false;
	}
	return true;
}

bool SrtlaDock::websocketSetLinkEnabled(std::uint64_t linkId, bool enabled, QString &error)
{
	for (int row = 0; row < links_->rowCount(); ++row) {
		auto *identity = links_->item(row, 1);
		auto *checkbox = links_->item(row, 0);
		if (!identity || !checkbox)
			continue;
		const auto key = identity->data(Qt::UserRole).toString();
		if (srtla_output_link_id(key.toUtf8().constData()) != linkId)
			continue;
		checkbox->setCheckState(enabled ? Qt::Checked : Qt::Unchecked);
		if (selected_[key.toStdString()] != enabled) {
			error = tr("The link could not be changed. The last enabled link cannot be disabled while output is running.");
			return false;
		}
		return true;
	}
	error = tr("Unknown link ID.");
	return false;
}

bool SrtlaDock::websocketSetBitrateControl(bool automatic, int manualBitrateKbps, QString &error)
{
	if (manualBitrateKbps < manualBitrate_->minimum() || manualBitrateKbps > manualBitrate_->maximum()) {
		error = tr("Manual bitrate must be between %1 and %2 kb/s.")
			.arg(manualBitrate_->minimum()).arg(manualBitrate_->maximum());
		return false;
	}
	if (running_ && output_ && srtla_output_set_bitrate_control(output_, automatic, manualBitrateKbps) != 0) {
		error = tr("The selected encoder cannot change bitrate while active.");
		return false;
	}
	const QSignalBlocker manualBlocker(manualBitrate_);
	const QSignalBlocker automaticBlocker(autoBitrate_);
	manualBitrate_->setValue(manualBitrateKbps);
	autoBitrate_->setChecked(automatic);
	manualBitrate_->setDisabled(automatic);
	if (settings_) {
		obs_data_set_bool(settings_, "auto_bitrate", automatic);
		obs_data_set_int(settings_, "bitrate", manualBitrateKbps);
	}
	return true;
}

bool SrtlaDock::websocketSetMaxBitrate(int maxBitrateKbps, QString &error)
{
	if (maxBitrateKbps < maxBitrate_->minimum() || maxBitrateKbps > maxBitrate_->maximum()) {
		error = tr("Maximum bitrate must be between %1 and %2 kb/s.")
			.arg(maxBitrate_->minimum()).arg(maxBitrate_->maximum());
		return false;
	}
	if (running_ && output_ && srtla_output_set_max_bitrate(output_, maxBitrateKbps) != 0) {
		error = tr("The selected encoder cannot change bitrate while active.");
		return false;
	}
	const QSignalBlocker maximumBlocker(maxBitrate_);
	maxBitrate_->setValue(maxBitrateKbps);
	if (settings_)
		obs_data_set_int(settings_, "max_bitrate", maxBitrateKbps);
	return true;
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
	for (size_t index = 0; index < metricValues_.size(); ++index) {
		metricValues_[index]->setText(QStringLiteral("--"));
		metricSparklines_[index]->setHistory({});
		metricHistory_[index].clear();
	}
	linkHistory_.clear();
	updateOverviewSparklines();
	updateHistoryPanel();
}

void SrtlaDock::refreshAdapters()
{
	const auto adapters = NetworkMonitor().enumerate();
	bool topologyChanged = links_->rowCount() != static_cast<int>(adapters.size());
	if (!topologyChanged) {
		for (int row = 0; row < links_->rowCount(); ++row) {
			const auto *identity = links_->item(row, 1);
			if (!identity || !links_->item(row, 0) || !links_->item(row, 2) ||
			    identity->data(Qt::UserRole).toString().toStdString() != adapters[static_cast<size_t>(row)].id) {
				topologyChanged = true;
				break;
			}
		}
	}

	if (!topologyChanged) {
		bool visibilityChanged = false;
		bool historyTitleChanged = false;
		{
			QSignalBlocker blocker(links_);
			for (int row = 0; row < links_->rowCount(); ++row) {
				const auto &adapter = adapters[static_cast<size_t>(row)];
				const auto selection = selected_.try_emplace(adapter.id, adapter.enabled).first;
				auto *use = links_->item(row, 0);
				const auto desiredCheckState = selection->second ? Qt::Checked : Qt::Unchecked;
				if (use && use->checkState() != desiredCheckState) {
					use->setCheckState(desiredCheckState);
					visibilityChanged = true;
				}
				auto *identity = links_->item(row, 1);
				const QString label = QString::fromStdString(adapter.label + " / " + adapter.address);
				if (identity && identity->text() != label) {
					identity->setText(label);
					identity->setToolTip(label);
					if (identity->data(Qt::UserRole).toString().toStdString() == selectedLinkKey_)
						historyTitleChanged = true;
				}
				if (auto *state = links_->item(row, 2))
					state->setText(adapter.operational ? tr("Ready") : tr("Offline"));
			}
		}
		if (visibilityChanged)
			applyAdapterVisibility();
		else if (historyTitleChanged)
			updateHistoryPanel();
		if (output_ && running_)
			srtla_output_update_adapters(output_);
		return;
	}

	const int verticalScrollPosition = links_->verticalScrollBar()->value();
	const int horizontalScrollPosition = links_->horizontalScrollBar()->value();
	{
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
			identity->setToolTip(QString::fromStdString(adapter.label + " / " + adapter.address));
			links_->setItem(row, 1, identity);
			auto *state = new QTableWidgetItem(adapter.operational ? tr("Ready") : tr("Offline"));
			links_->setItem(row, 2, state);
			auto &history = linkHistory_[adapter.id];
			set_telemetry_cell(links_, row, 3, QStringLiteral("--"), history[0], {}, historyWindowSeconds_);
			for (int column = 4; column < 9; ++column)
				set_telemetry_cell(links_, row, column, QStringLiteral("--"), history[static_cast<size_t>(column - 3)], {}, historyWindowSeconds_);
			links_->setRowHeight(row, 46);
		}
		applyAdapterVisibility();
		int selectedRow = -1;
		int firstSelectedRow = -1;
		for (int row = 0; row < links_->rowCount(); ++row) {
			auto *identity = links_->item(row, 1);
			if (!identity)
				continue;
			const bool selected = links_->item(row, 0)->checkState() == Qt::Checked;
			if (selected && firstSelectedRow < 0)
				firstSelectedRow = row;
			if (identity->data(Qt::UserRole).toString().toStdString() == selectedLinkKey_ && !links_->isRowHidden(row))
				selectedRow = row;
		}
		if (selectedRow < 0)
			selectedRow = firstSelectedRow;
		if (selectedRow < 0 && links_->rowCount() > 0)
			selectedRow = 0;
		if (selectedRow >= 0) {
			links_->setCurrentCell(selectedRow, 1);
			links_->selectRow(selectedRow);
			if (const auto *identity = links_->item(selectedRow, 1))
				selectedLinkKey_ = identity->data(Qt::UserRole).toString().toStdString();
		} else {
			selectedLinkKey_.clear();
		}
		if (output_ && running_)
			srtla_output_update_adapters(output_);
		updateHistoryPanel();
	}
	QTimer::singleShot(0, links_, [this, verticalScrollPosition, horizontalScrollPosition] {
		links_->verticalScrollBar()->setValue(verticalScrollPosition);
		links_->horizontalScrollBar()->setValue(horizontalScrollPosition);
	});
}

void SrtlaDock::resizeEvent(QResizeEvent *event)
{
	QWidget::resizeEvent(event);
	arrangeOverview();
}

void SrtlaDock::arrangeOverview()
{
	if (!overviewLayout_)
		return;
	const int columns = width() >= 880 ? 6 : (width() >= 540 ? 3 : 2);
	for (size_t index = 0; index < metricTiles_.size(); ++index) {
		auto *tile = metricTiles_[index];
		if (!tile)
			continue;
		overviewLayout_->removeWidget(tile);
		overviewLayout_->addWidget(tile, static_cast<int>(index) / columns, static_cast<int>(index) % columns);
	}
	for (int column = 0; column < columns; ++column)
		overviewLayout_->setColumnStretch(column, 1);
}

void SrtlaDock::applyAdapterVisibility()
{
	if (!links_ || !showAllAdapters_)
		return;
	int selectedCount = 0;
	for (int row = 0; row < links_->rowCount(); ++row) {
		const auto *use = links_->item(row, 0);
		if (use && use->checkState() == Qt::Checked)
			++selectedCount;
	}
	const bool showAll = showAllAdapters_->isChecked() || selectedCount == 0;
	int visibleCount = 0;
	{
		QSignalBlocker blocker(links_);
		for (int row = 0; row < links_->rowCount(); ++row) {
			const auto *use = links_->item(row, 0);
			const bool selected = use && use->checkState() == Qt::Checked;
			const bool visible = showAll || selected;
			links_->setRowHidden(row, !visible);
			if (visible)
				++visibleCount;
		}
	}
	adapterCount_->setText(selectedCount == 0 ? tr("· none selected") : tr("· %1 selected").arg(selectedCount));
	showAllAdapters_->setText(tr("Show all adapters (%1)").arg(links_->rowCount()));
	const int headerHeight = links_->horizontalHeader()->height();
	const int rowHeight = links_->verticalHeader()->defaultSectionSize();
	links_->setMaximumHeight(headerHeight + visibleCount * rowHeight + 2 * links_->frameWidth() + 8);
	links_->setMinimumHeight(headerHeight + 2 * links_->frameWidth() + 8);
	if (trendHint_)
		trendHint_->setText(tr("Inline trends · %1 min").arg(historyWindowSeconds_ / 60));

	int currentRow = links_->currentRow();
	if (currentRow < 0 || links_->isRowHidden(currentRow)) {
		currentRow = -1;
		for (int row = 0; row < links_->rowCount(); ++row) {
			if (!links_->isRowHidden(row)) {
				currentRow = row;
				break;
			}
		}
		if (currentRow >= 0) {
			QSignalBlocker blocker(links_);
			links_->setCurrentCell(currentRow, 1);
			links_->selectRow(currentRow);
		}
	}
	if (currentRow >= 0) {
		if (const auto *identity = links_->item(currentRow, 1))
			selectedLinkKey_ = identity->data(Qt::UserRole).toString().toStdString();
	}
	updateHistoryPanel();
}

void SrtlaDock::updateOverviewSparklines()
{
	if (!metricSparklines_[0])
		return;
	const int windowSeconds = std::max(1, historyWindowSeconds_);
	const QString minutes = QString::number(windowSeconds / 60);
	if (overviewTitle_)
		overviewTitle_->setText(tr("Output overview · last %1 min").arg(minutes));
	for (size_t index = 0; index < metricHistory_.size(); ++index) {
		const auto values = history_tail(metricHistory_[index], windowSeconds);
		if (index == 0 || index == 5)
			metricSparklines_[index]->setHistory(values, {}, true, windowSeconds);
		else if (index == 3)
			metricSparklines_[index]->setHistory(values, history_tail(metricHistory_[4], windowSeconds), false, windowSeconds);
		else
			metricSparklines_[index]->setHistory(values, {}, false, windowSeconds);
	}
}

void SrtlaDock::updateHistoryPanel()
{
	if (!historyTitle_ || !historyCharts_[0])
		return;
	QString adapterName = tr("No adapter selected");
	auto found = linkHistory_.find(selectedLinkKey_);
	if (!selectedLinkKey_.empty() && links_) {
		for (int row = 0; row < links_->rowCount(); ++row) {
			const auto *identity = links_->item(row, 1);
			if (identity && identity->data(Qt::UserRole).toString().toStdString() == selectedLinkKey_) {
				adapterName = identity->text();
				break;
			}
		}
	}
	historyTitle_->setText(tr("%1 · history").arg(adapterName));
	const auto noHistory = std::array<std::deque<double>, 6>{};
	const auto &history = found == linkHistory_.end() ? noHistory : found->second;
	if (history[3].empty() || history[4].empty() || history[5].empty()) {
		historyNow_->setText(tr("No history yet · start output to collect link telemetry"));
	} else {
		historyNow_->setText(tr("Now · Offered %1 Mb/s · Delivered %2 Mb/s · CC target %3 Mb/s · RTT %4 ms · NAK %5%")
			.arg(history[3].back(), 0, 'f', 1)
			.arg(history[4].back(), 0, 'f', 1)
			.arg(history[5].back(), 0, 'f', 1)
			.arg(history[1].empty() ? 0.0 : history[1].back(), 0, 'f', 0)
			.arg(history[2].empty() ? 0.0 : history[2].back(), 0, 'f', 1));
	}
	const std::vector<HistoryChartWidget::Series> bandwidth = {
		{tr("Offered"), history_tail(history[3], historyWindowSeconds_), QColor(184, 196, 216), Qt::SolidLine},
		{tr("Delivered"), history_tail(history[4], historyWindowSeconds_), QColor(111, 199, 164), Qt::SolidLine},
		{tr("CC target"), history_tail(history[5], historyWindowSeconds_), QColor(225, 173, 82), Qt::DashLine}};
	const std::vector<HistoryChartWidget::Series> rtt = {
		{tr("RTT"), history_tail(history[1], historyWindowSeconds_), QColor(130, 185, 255), Qt::SolidLine}};
	const std::vector<HistoryChartWidget::Series> nak = {
		{tr("NAK rate"), history_tail(history[2], historyWindowSeconds_), QColor(199, 146, 234), Qt::SolidLine}};
	historyCharts_[0]->setSeries(bandwidth, historyWindowSeconds_);
	historyCharts_[1]->setSeries(rtt, historyWindowSeconds_);
	historyCharts_[2]->setSeries(nak, historyWindowSeconds_);
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
		output_ = obs_output_create("srtla_output", "SRTLA Output", settings_, nullptr);
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
	const auto appendMetric = [this](size_t index, double value) {
		append_sample(metricHistory_[index], value);
	};
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
			auto &history = linkHistory_[identity->data(Qt::UserRole).toString().toStdString()];
			const auto quality = link.value(QStringLiteral("quality_percent")).toInt();
			const auto rttMs = link.value(QStringLiteral("rtt_ms")).toInt();
			const auto nakRate = link.value(QStringLiteral("loss_permille")).toInt() / 10.0;
			const auto linkOffered = link.value(QStringLiteral("used_bps")).toInteger();
			const auto linkDelivered = link.value(QStringLiteral("delivered_bps")).toInteger();
			const auto target = link.value(QStringLiteral("target_bps")).toInteger();
			append_sample(history[0], quality);
			append_sample(history[1], rttMs);
			append_sample(history[2], nakRate);
			append_sample(history[3], static_cast<double>(linkOffered) / 1000000.0);
			append_sample(history[4], static_cast<double>(linkDelivered) / 1000000.0);
			append_sample(history[5], static_cast<double>(target) / 1000000.0);
			set_telemetry_cell(links_, row, 3, QString::number(quality), history[0], {}, historyWindowSeconds_);
			set_telemetry_cell(links_, row, 4, QString::number(rttMs), history[1], {}, historyWindowSeconds_);
			set_telemetry_cell(links_, row, 5, QString::number(nakRate, 'f', 1), history[2], {}, historyWindowSeconds_);
			set_telemetry_cell(links_, row, 6, QString::number(static_cast<double>(linkOffered) / 1000000.0, 'f', 1), history[3], {}, historyWindowSeconds_);
			set_telemetry_cell(links_, row, 7, QString::number(static_cast<double>(linkDelivered) / 1000000.0, 'f', 1), history[4], {}, historyWindowSeconds_);
			set_telemetry_cell(links_, row, 8, QString::number(static_cast<double>(target) / 1000000.0, 'f', 1), history[5], {}, historyWindowSeconds_);
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
	const auto abrState = root.value(QStringLiteral("abr_state")).toString();
	const bool srtReady = root.value(QStringLiteral("srt_stats_ready")).toBool();
	const double rttValue = srtReady ? static_cast<double>(root.value(QStringLiteral("srt_rtt_ms")).toInteger()) :
		std::numeric_limits<double>::quiet_NaN();
	const double queueValue = srtReady ? static_cast<double>(root.value(QStringLiteral("srt_send_buffer_ms")).toInteger()) :
		std::numeric_limits<double>::quiet_NaN();
	const double videoValue = static_cast<double>(root.value(QStringLiteral("current_video_bps")).toInteger()) / 1000000.0;
	const double targetValue = static_cast<double>(root.value(QStringLiteral("recommended_video_bps")).toInteger()) / 1000000.0;
	appendMetric(0, abr_sparkline_level(abrState));
	appendMetric(1, rttValue);
	appendMetric(2, queueValue);
	appendMetric(3, videoValue);
	appendMetric(4, targetValue);
	appendMetric(5, active);
	metricValues_[0]->setText(abr_sparkline_label(abrState));
	metricValues_[0]->setToolTip(abrState);
	metricValues_[1]->setText(srtReady ? QStringLiteral("%1 / %2").arg(root.value(QStringLiteral("srt_rtt_ms")).toInteger()).arg(root.value(QStringLiteral("srt_latency_ms")).toInteger()) : QStringLiteral("--"));
	metricValues_[1]->setToolTip(tr("Measured SRT RTT / configured latency, in milliseconds"));
	metricValues_[2]->setText(srtReady ? tr("%1 pkt · %2 ms").arg(root.value(QStringLiteral("srt_send_buffer_packets")).toInteger()).arg(root.value(QStringLiteral("srt_send_buffer_ms")).toInteger()) : QStringLiteral("--"));
	metricValues_[2]->setToolTip(tr("Current SRT sender queue: packet count and time"));
	metricValues_[3]->setText(QStringLiteral("%1 Mb/s").arg(videoValue, 0, 'f', 1));
	metricValues_[4]->setText(QStringLiteral("%1 Mb/s").arg(targetValue, 0, 'f', 1));
	metricValues_[5]->setText(tr("%1 / %2").arg(active).arg(links.size()));
	metricValues_[5]->setToolTip(tr("Active network paths / all configured paths"));
	updateOverviewSparklines();
	metricSparklines_[0]->setToolTip(tr("Queue thresholds L/H/S: %1/%2/%3 packets | RTT grow below: %4 ms | RTT reduce above: %5 ms | SRT estimate: %6 kb/s | send: %7 kb/s | link capacity: %8 kb/s | offered: %9 kb/s | delivered: %10 kb/s | retransmissions: %11% | sender loss: %12 | dropped: %13 bytes")
		.arg(root.value(QStringLiteral("abr_queue_light_packets")).toInteger())
		.arg(root.value(QStringLiteral("abr_queue_heavy_packets")).toInteger())
		.arg(root.value(QStringLiteral("abr_queue_severe_packets")).toInteger())
		.arg(root.value(QStringLiteral("abr_rtt_increase_below_ms")).toInteger())
		.arg(root.value(QStringLiteral("abr_rtt_decrease_above_ms")).toInteger())
		.arg(srtRaw / 1000)
		.arg(root.value(QStringLiteral("srt_send_rate_bps")).toInteger() / 1000)
		.arg(linkCapacity / 1000)
		.arg(offered / 1000)
		.arg(delivered / 1000)
		.arg(root.value(QStringLiteral("srt_retransmit_permille")).toInteger() / 10.0, 0, 'f', 1)
		.arg(root.value(QStringLiteral("srt_sender_loss_packets")).toInteger())
		.arg(root.value(QStringLiteral("srt_dropped_bytes")).toInteger()));
	const auto state = root.value(QStringLiteral("state")).toString(active > 0 ? tr("Live") : tr("Waiting for network"));
	state_->setText(state);
	const auto error = root.value(QStringLiteral("error")).toString();
	state_->setToolTip(error);
	updateHistoryPanel();
	const auto vendorStatus = websocketStatusJson();
	if (!vendorStatus.isEmpty())
		srtla_websocket_emit_status_json(vendorStatus.constData());
}

#pragma once

#include <QWidget>
#include <QByteArray>
#include <QString>

#include <array>
#include <cstdint>
#include <deque>
#include <memory>
#include <string>
#include <unordered_map>

class QCheckBox;
class QComboBox;
class QGridLayout;
class QLabel;
class QLineEdit;
class QPushButton;
class QSpinBox;
class QTableWidget;
class QTimer;
class QResizeEvent;
class QToolButton;
class HistoryChartWidget;
class SparklineWidget;

class SrtlaDock final : public QWidget {
public:
	explicit SrtlaDock(QWidget *parent = nullptr);
	~SrtlaDock() override;

	void refreshAdapters();
	void stopOutput();
	void reloadProfile();
	bool sharesEncoderWith(struct obs_output *other) const;
	QByteArray websocketStatusJson();
	bool websocketSetOutputActive(bool active, QString &error);
	bool websocketSetLinkEnabled(std::uint64_t linkId, bool enabled, QString &error);
	bool websocketSetBitrateControl(bool automatic, int manualBitrateKbps, QString &error);
	bool websocketSetMaxBitrate(int maxBitrateKbps, QString &error);

protected:
	void resizeEvent(QResizeEvent *event) override;

private:
	void toggleOutput();
	void refreshStatus();
	void arrangeOverview();
	void applyAdapterVisibility();
	void updateOverviewSparklines();
	void updateHistoryPanel();
	void saveSelectedLinks();
	void loadProfile();
	bool saveProfile();

	QPushButton *startStop_ = nullptr;
	QLineEdit *url_ = nullptr;
	QLineEdit *streamId_ = nullptr;
	QLineEdit *passphrase_ = nullptr;
	QLabel *secret_warning_ = nullptr;
	QComboBox *encoderSource_ = nullptr;
	QComboBox *customVideoEncoder_ = nullptr;
	QComboBox *customAudioEncoder_ = nullptr;
	QCheckBox *autoBitrate_ = nullptr;
	QSpinBox *manualBitrate_ = nullptr;
	QSpinBox *maxBitrate_ = nullptr;
	QLabel *state_ = nullptr;
	QLabel *overviewTitle_ = nullptr;
	QLabel *trendHint_ = nullptr;
	QLabel *settingsSummary_ = nullptr;
	QWidget *connectionSettings_ = nullptr;
	QGridLayout *overviewLayout_ = nullptr;
	std::array<QWidget *, 6> metricTiles_{};
	std::array<QLabel *, 6> metricValues_{};
	std::array<SparklineWidget *, 6> metricSparklines_{};
	std::array<std::deque<double>, 6> metricHistory_{};
	QTableWidget *links_ = nullptr;
	QCheckBox *showAllAdapters_ = nullptr;
	QLabel *adapterCount_ = nullptr;
	QLabel *historyTitle_ = nullptr;
	QLabel *historyNow_ = nullptr;
	QComboBox *historyRange_ = nullptr;
	QToolButton *historyCollapse_ = nullptr;
	std::array<HistoryChartWidget *, 3> historyCharts_{};
	QWidget *historyPanel_ = nullptr;
	std::unordered_map<std::string, std::array<std::deque<double>, 6>> linkHistory_;
	std::string selectedLinkKey_;
	int historyWindowSeconds_ = 60;
	QTimer *timer_ = nullptr;
	bool running_ = false;
	bool secret_error_ = false;
	int latency_ms_ = 2000;
	int pbkeylen_ = 16;
	std::unordered_map<std::string, bool> selected_;
	struct obs_output *output_ = nullptr;
	struct obs_data *settings_ = nullptr;
};

#pragma once

#include <QWidget>

#include <memory>
#include <string>
#include <unordered_map>

class QCheckBox;
class QComboBox;
class QLabel;
class QLineEdit;
class QPushButton;
class QSpinBox;
class QTableWidget;
class QTimer;

class SrtlaDock final : public QWidget {
public:
	explicit SrtlaDock(QWidget *parent = nullptr);
	~SrtlaDock() override;

	void refreshAdapters();
	void stopOutput();

private:
	void toggleOutput();
	void refreshStatus();

	QPushButton *startStop_ = nullptr;
	QLineEdit *url_ = nullptr;
	QComboBox *encoderSource_ = nullptr;
	QComboBox *customVideoEncoder_ = nullptr;
	QComboBox *customAudioEncoder_ = nullptr;
	QCheckBox *autoBitrate_ = nullptr;
	QSpinBox *manualBitrate_ = nullptr;
	QSpinBox *maxBitrate_ = nullptr;
	QLabel *state_ = nullptr;
	QLabel *metrics_ = nullptr;
	QTableWidget *links_ = nullptr;
	QTimer *timer_ = nullptr;
	bool running_ = false;
	std::unordered_map<std::string, bool> selected_;
	struct obs_output *output_ = nullptr;
	struct obs_data *settings_ = nullptr;
};

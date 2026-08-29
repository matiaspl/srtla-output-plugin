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
	void reloadProfile();
	bool sharesEncoderWith(struct obs_output *other) const;

private:
	void toggleOutput();
	void refreshStatus();
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
	QLabel *metrics_ = nullptr;
	QTableWidget *links_ = nullptr;
	QTimer *timer_ = nullptr;
	bool running_ = false;
	bool secret_error_ = false;
	int latency_ms_ = 2000;
	int pbkeylen_ = 16;
	std::unordered_map<std::string, bool> selected_;
	struct obs_output *output_ = nullptr;
	struct obs_data *settings_ = nullptr;
};

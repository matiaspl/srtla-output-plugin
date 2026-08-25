#pragma once

#include <QWidget>

#include <memory>
#include <string>
#include <unordered_map>

class QCheckBox;
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
	QCheckBox *autoBitrate_ = nullptr;
	QSpinBox *manualBitrate_ = nullptr;
	QLabel *state_ = nullptr;
	QLabel *metrics_ = nullptr;
	QTableWidget *links_ = nullptr;
	QTimer *timer_ = nullptr;
	bool running_ = false;
	std::unordered_map<std::string, bool> selected_;
	struct obs_output *output_ = nullptr;
	struct obs_data *settings_ = nullptr;
};

#pragma once

#include <functional>
#include <string>
#include <utility>

// Small dependency-free seam around the OBS capture calls.  The production
// output supplies lambdas that call OBS; tests supply recording lambdas and can
// verify ordering and idempotent teardown without starting an OBS instance.
class OutputCaptureLifecycle final {
public:
	using CanBegin = std::function<bool()>;
	using Initialize = std::function<bool()>;
	using Begin = std::function<bool()>;
	using End = std::function<void()>;
	using WaitEnd = std::function<void()>;
	using StopSignal = std::function<void()>;

	OutputCaptureLifecycle(CanBegin can_begin, Initialize initialize, Begin begin, End end,
	                      StopSignal stop_signal = {}, WaitEnd wait_end = {})
		: can_begin_(std::move(can_begin)), initialize_(std::move(initialize)), begin_(std::move(begin)),
		  end_(std::move(end)), wait_end_(std::move(wait_end)), stop_signal_(std::move(stop_signal)) {}

	bool prepare(std::string &error)
	{
		if (prepared_)
			return true;
		if (!can_begin_ || !can_begin_()) {
			error = "OBS cannot begin encoded data capture";
			return false;
		}
		prepared_ = true;
		return true;
	}

	bool initialize_and_begin(std::string &error)
	{
		if (!prepared_) {
			error = "OBS capture was not prepared";
			return false;
		}
		if (active_)
			return true;
		if (!initialize_ || !initialize_()) {
			error = "OBS encoder initialization failed";
			return false;
		}
		if (!begin_ || !begin_()) {
			error = "OBS data capture failed";
			return false;
		}
		active_ = true;
		return true;
	}

	void end()
	{
		if (!active_)
			return;
		active_ = false;
		if (end_)
			end_();
		if (wait_end_)
			wait_end_();
		if (stop_signal_)
			stop_signal_();
	}

	bool active() const { return active_; }

private:
	CanBegin can_begin_;
	Initialize initialize_;
	Begin begin_;
	End end_;
	WaitEnd wait_end_;
	StopSignal stop_signal_;
	bool prepared_ = false;
	bool active_ = false;
};

#include "../plugin/output-capture-lifecycle.hpp"

#include <cassert>
#include <string>
#include <vector>

int main()
{
	std::vector<std::string> calls;
	OutputCaptureLifecycle lifecycle(
		[&] { calls.emplace_back("can"); return true; },
		[&] { calls.emplace_back("initialize"); return true; },
		[&] { calls.emplace_back("begin"); return true; },
		[&] { calls.emplace_back("end"); },
		[&] { calls.emplace_back("stop"); });
	std::string error;
	assert(lifecycle.prepare(error));
	assert(lifecycle.initialize_and_begin(error));
	assert(lifecycle.active());
	lifecycle.end();
	lifecycle.end();
	assert((calls == std::vector<std::string>{"can", "initialize", "begin", "end", "stop"}));

	calls.clear();
	OutputCaptureLifecycle failure(
		[&] { calls.emplace_back("can"); return true; },
		[&] { calls.emplace_back("initialize"); return false; },
		[&] { calls.emplace_back("begin"); return true; },
		[&] { calls.emplace_back("end"); },
		[&] { calls.emplace_back("stop"); });
	assert(failure.prepare(error));
	assert(!failure.initialize_and_begin(error));
	assert(error == "OBS encoder initialization failed");
	assert((calls == std::vector<std::string>{"can", "initialize"}));

	calls.clear();
	OutputCaptureLifecycle begin_failure(
		[&] { calls.emplace_back("can"); return true; },
		[&] { calls.emplace_back("initialize"); return true; },
		[&] { calls.emplace_back("begin"); return false; },
		[&] { calls.emplace_back("end"); },
		[&] { calls.emplace_back("stop"); });
	assert(begin_failure.prepare(error));
	assert(!begin_failure.initialize_and_begin(error));
	assert(error == "OBS data capture failed");
	begin_failure.end();
	assert((calls == std::vector<std::string>{"can", "initialize", "begin"}));
	return 0;
}

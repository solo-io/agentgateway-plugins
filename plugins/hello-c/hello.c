// A minimal agentgateway plugin in C, written directly against the ABI.
// It adds `x-hello: world` to every request before it is sent upstream.
#include "agentgateway_abi.h"

#define STR(s) {(const uint8_t *)(s), sizeof(s) - 1}

// The host vtable is only valid during create, so keep the callback we need.
static AgwHostSetHeader set_header;

// The work is synchronous, so the future completes on its first poll. Its
// state is just the request context.
static uint32_t poll(void *ctx, const AgwRawWaker *waker, AgwApplyResult *result) {
	(void)waker;
	set_header(ctx, (AgwString)STR("x-hello"), (AgwBytes)STR("world"));
	result->status = AGW_APPLY_CONTINUE;
	return AGW_POLL_READY;
}

static AgwApplyFuture apply(void *state, const AgwContext *ctx) {
	(void)state;
	return (AgwApplyFuture){.state = (void *)ctx, .poll = poll};
}

static uint32_t create(AgwBytes config, const AgwHostVTable *host, AgwPluginInstance *instance, AgwError *err) {
	(void)config;
	(void)err;
	set_header = host->set_header;
	instance->apply = apply;
	return AGW_STATUS_OK;
}

static const AgwPluginDescriptor descriptor = {
	.size = sizeof(AgwPluginDescriptor),
	.abi_version = AGW_PLUGIN_ABI_VERSION,
	.name = STR("hello-c"),
	.version = STR("0.1.0"),
	.create = create,
};

__attribute__((visibility("default"))) const AgwPluginDescriptor *agentgateway_plugin_entrypoint(void) {
	return &descriptor;
}

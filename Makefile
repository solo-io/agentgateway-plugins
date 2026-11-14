# Each Rust plugin is its own Cargo workspace, so these targets run cargo once
# per plugin. C plugins have their own Makefile.
PLUGINS := $(patsubst %/Cargo.toml,%,$(wildcard plugins/*/Cargo.toml))
C_PLUGINS := $(patsubst %/Makefile,%,$(wildcard plugins/*/Makefile))

define each
	@set -e; for p in $(PLUGINS); do echo "==> $$p"; (cd $$p && $(1)); done
endef

.PHONY: build test lint fmt
build:
	$(call each,cargo build --release)
	@set -e; for p in $(C_PLUGINS); do echo "==> $$p"; $(MAKE) -C $$p; done
test:
	$(call each,cargo test)
lint:
	$(call each,cargo clippy --all-targets -- -D warnings)
fmt:
	$(call each,cargo fmt)

# Build a plugin image: make docker PLUGIN=regorus [TAG=regorus:dev]
.PHONY: docker
docker:
	@test -n "$(PLUGIN)" || (echo "usage: make docker PLUGIN=<name> [TAG=<image>]" >&2; exit 1)
	docker build --build-arg PLUGIN=$(PLUGIN) -t $(or $(TAG),$(PLUGIN):dev) .

# Run a plugin in a real enterprise agentgateway: make e2e PLUGIN=regorus [AGW_BIN=path]
.PHONY: e2e
e2e:
	@test -n "$(PLUGIN)" || (echo "usage: make e2e PLUGIN=<name> [AGW_BIN=<agentgateway binary>]" >&2; exit 1)
	./scripts/e2e.sh $(PLUGIN) $(AGW_BIN)

# Builds a plugin (Rust, or C with a Makefile) and packages it as a minimal OCI image containing only
# /plugin.so.
#
#   docker build --build-arg PLUGIN=regorus -t regorus .
#   docker buildx build --platform linux/amd64,linux/arm64 --build-arg PLUGIN=regorus .
#
# The builder runs natively and cross-compiles for the target platform, so
# multi-arch builds don't need emulation. Building on bookworm keeps the glibc
# requirement low enough for the agentgateway runtime image.
FROM --platform=$BUILDPLATFORM docker.io/library/rust:1.99.0-bookworm AS builder
ARG TARGETARCH
RUN case "${TARGETARCH}" in \
      amd64) arch=x86_64; gcc=gcc-x86-64-linux-gnu ;; \
      arm64) arch=aarch64; gcc=gcc-aarch64-linux-gnu ;; \
      *) echo "unsupported arch ${TARGETARCH}" >&2; exit 1 ;; \
    esac \
 && echo "${arch}" > /arch \
 && apt-get update \
 && apt-get install -y --no-install-recommends "${gcc}" "libc6-dev-${TARGETARCH}-cross" \
 && rm -rf /var/lib/apt/lists/* \
 && rustup target add "${arch}-unknown-linux-gnu"

WORKDIR /src
COPY rust rust
COPY agentgateway_abi.h .
ARG PLUGIN
COPY plugins/${PLUGIN} plugins/${PLUGIN}
WORKDIR /src/plugins/${PLUGIN}
# Per-arch caches: multi-arch builds run in parallel on the same host, and
# concurrent cargo processes sharing a registry cache race while unpacking.
RUN --mount=type=cache,id=cargo-registry-${TARGETARCH},target=/usr/local/cargo/registry \
    --mount=type=cache,id=cargo-target-${PLUGIN}-${TARGETARCH},target=/src/plugins/${PLUGIN}/target \
    arch="$(cat /arch)" \
 && if [ -f Cargo.toml ]; then \
      export CARGO_TARGET_$(echo "${arch}" | tr a-z A-Z)_UNKNOWN_LINUX_GNU_LINKER="${arch}-linux-gnu-gcc" \
             CC_${arch}_unknown_linux_gnu="${arch}-linux-gnu-gcc" \
      && cargo build --release --target "${arch}-unknown-linux-gnu" \
      && cp target/"${arch}"-unknown-linux-gnu/release/lib*.so /plugin.so; \
    else \
      make CC="${arch}-linux-gnu-gcc" && cp plugin.so /plugin.so; \
    fi

FROM scratch
COPY --from=builder /plugin.so /plugin.so

#!/usr/bin/env bash
# Copies the Rust plugin SDK and ABI header from an agentgateway checkout.
#
# Usage: ./sync-sdk.sh [path-to-agentgateway]   (default: ../agentgateway-enterprise)
#
# Only the crates are copied. rust/Cargo.toml is maintained here.
set -euo pipefail

src="${1:-../agentgateway-enterprise}"
cd "$(dirname "$0")"

# Check before deleting anything, so a wrong checkout leaves rust/ intact.
for crate in abi sdk; do
  if ! git -C "${src}" cat-file -e "HEAD:crates/${crate}" 2>/dev/null; then
    echo "error: ${src} @ $(git -C "${src}" rev-parse --abbrev-ref HEAD 2>/dev/null || echo '?') has no crates/${crate}" >&2
    echo "pass the checkout that has the plugin SDK, e.g. ./sync-sdk.sh <path-to-agentgateway-checkout>" >&2
    exit 1
  fi
done

for crate in abi sdk; do
  rm -rf "rust/${crate}"
  mkdir -p "rust/${crate}"
  # -m: use the current time, so cargo notices the SDK changed and rebuilds it.
  git -C "${src}" archive HEAD "crates/${crate}" | tar -x -m --strip-components=2 --exclude=AGENTS.md -C "rust/${crate}"
done
# The C header is the language-neutral ABI, so it lives at the top level.
mv rust/abi/agentgateway_abi.h agentgateway_abi.h

echo "synced from ${src} @ $(git -C "${src}" rev-parse --short HEAD)"

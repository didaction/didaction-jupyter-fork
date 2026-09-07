#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
cargo check --target wasm32-unknown-unknown -p notebook-protocol -p notebook-core -p notebook-runtime
pnpm run build:wasm
pnpm run format
pnpm run typecheck
pnpm test
pnpm audit:licenses
uv run ruff format --check services scripts tests deploy
uv run ruff check services scripts tests deploy
uv run mypy
uv run pytest -q
uv run python -m pip --version
pnpm prepare:browser-kernel
pnpm run build
pnpm run test:browser
pnpm run test:browser-kernel
pnpm run build:browser
pnpm run test:browser-static
DIDACTION_GATEWAY_IMPLEMENTATION=python scripts/smoke.sh
DIDACTION_GATEWAY_IMPLEMENTATION=rust DIDACTION_NATIVE_BROWSER_CHECK=1 scripts/smoke.sh

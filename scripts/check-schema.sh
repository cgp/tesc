#!/usr/bin/env bash
# Fail if the committed JSON Schemas differ from what the Rust types generate.
#
# The plan format is the one contract between the engine and the API (see
# docs/design-api-engine-contract.md). Rust types are authoritative; schema/*.json is
# generated and committed so the API can validate against it without building Rust.
# If this fails, run:  cargo run -p metrix-engine -- --emit-schemas schema
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

cargo run --quiet --manifest-path "$root/engine/Cargo.toml" \
    -p metrix-engine -- --emit-schemas "$tmp" >/dev/null

status=0
for generated in "$tmp"/*.json; do
    name="$(basename "$generated")"
    committed="$root/schema/$name"

    if [[ ! -f "$committed" ]]; then
        echo "MISSING  schema/$name is generated but not committed"
        status=1
        continue
    fi
    if ! diff -q "$committed" "$generated" >/dev/null; then
        echo "DRIFT    schema/$name does not match the Rust types:"
        diff -u "$committed" "$generated" | sed 's/^/         /' | head -40
        status=1
    fi
done

# A committed schema with no generator behind it is just as broken.
for committed in "$root"/schema/*.json; do
    name="$(basename "$committed")"
    [[ -f "$tmp/$name" ]] || { echo "ORPHAN   schema/$name has no type generating it"; status=1; }
done

if [[ $status -eq 0 ]]; then
    echo "schemas match the Rust types"
else
    echo
    echo "Regenerate with: cargo run -p metrix-engine -- --emit-schemas schema"
fi
exit $status

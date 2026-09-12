#!/usr/bin/env bash
# Everything that must be green. CI runs exactly this script, so what passes here
# passes there -- there is no second list of checks to drift out of sync.
#
#   scripts/check.sh            everything
#   scripts/check.sh engine     Rust only
#   scripts/check.sh api        Python only
#   scripts/check.sh contract   schemas and the rules that must not erode
set -uo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
scope="${1:-all}"
failed=()

run() {
    local name="$1"
    shift
    printf '\n\033[1m==> %s\033[0m\n' "$name"
    if "$@"; then
        return 0
    fi
    failed+=("$name")
    return 1
}

# ---------------------------------------------------------------- engine (Rust)
if [[ $scope == all || $scope == engine ]]; then
    # cd rather than --manifest-path: that flag belongs to the subcommand, and
    # `cargo fmt` does not take it at all.
    in_engine() { (cd "$root/engine" && "$@"); }
    run "engine: fmt" in_engine cargo fmt --check
    run "engine: clippy" in_engine cargo clippy --all-targets --all-features -- -D warnings
    run "engine: test" in_engine cargo test --quiet
fi

# --------------------------------------------------------------- api (Python)
if [[ $scope == all || $scope == api ]]; then
    run "api: ruff" uv run --project "$root/api" ruff check "$root/api"
    run "api: pytest" uv run --project "$root/api" pytest -q "$root/api"
fi

# ------------------------------------------------------------------- contract
if [[ $scope == all || $scope == contract ]]; then
    run "contract: schemas match the Rust types" bash "$root/scripts/check-schema.sh"

    # Rule 1/2 of both implementation plans: the engine gets concrete addresses and
    # never a cloud identity. Cheap to check, and the kind of thing that erodes one
    # convenient import at a time.
    run "contract: boto3 stays inside discovery/" bash -c '
        root="$1"
        offenders=$(grep -rln --include="*.py" -E "^[[:space:]]*(import|from)[[:space:]]+boto3" \
            "$root/api/src" 2>/dev/null | grep -v "/discovery/" || true)
        if [[ -n $offenders ]]; then
            echo "boto3 imported outside api/src/metrix_api/discovery/:"
            echo "$offenders" | sed "s/^/  /"
            exit 1
        fi
        echo "ok"
    ' _ "$root"

    # The engine must stay buildable and shippable on its own.
    run "contract: engine has no path dependency on the API" bash -c '
        root="$1"
        if grep -rn "metrix_api\|fastapi\|boto3" "$root/engine/crates" --include="*.toml" 2>/dev/null; then
            echo "the engine must not depend on the control plane"
            exit 1
        fi
        echo "ok"
    ' _ "$root"

    # An empty tracked source file is never intentional, and it fails in a way that
    # points somewhere else -- a truncated module reads as a routing bug. Cheap to
    # check, and it has already caught one file emptied by a careless rewrite.
    run "contract: no tracked source file is empty" bash -c '
        root="$1"
        cd "$root" || exit 1
        empty=$(git ls-files -- "*.py" "*.rs" "*.js" "*.css" "*.html" "*.sql" "*.sh" "*.toml"             | while read -r f; do [[ -f $f && ! -s $f ]] && echo "$f"; done)
        if [[ -n $empty ]]; then
            echo "empty tracked source file(s):"
            echo "$empty" | sed "s/^/  /"
            exit 1
        fi
        echo "ok"
    ' _ "$root"

    # The front end must draw itself without the public internet: this tool watches
    # private networks, and a menu that renders only when a CDN answers is a menu
    # that does not render on the box that needs it. Tabler is vendored under
    # api/web/vendor/ -- see the README there.
    run "contract: the front end fetches nothing" bash -c '
        root="$1"
        pattern="(src|href)=\"https?:|url\([\"'"'"']?https?:|@import[^;]*https?:"
        offenders=$(grep -rnE "$pattern" \
            "$root/api/web/index.html" "$root/api/web/css" "$root/api/web/js" || true)
        if [[ -n $offenders ]]; then
            echo "the page would fetch from the network at render time:"
            echo "$offenders" | sed "s/^/  /"
            exit 1
        fi
        echo "ok"
    ' _ "$root"

fi

# ----------------------------------------------------------------------- report
printf '\n'
if ((${#failed[@]} == 0)); then
    printf '\033[32mall checks passed\033[0m\n'
    exit 0
fi
printf '\033[31mfailed:\033[0m\n'
printf '  %s\n' "${failed[@]}"
exit 1

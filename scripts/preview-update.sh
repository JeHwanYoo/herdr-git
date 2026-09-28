#!/bin/sh
set -eu

project_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
preview_dir=$(mktemp -d "${TMPDIR:-/tmp}/herdr-git-update-preview.XXXXXX")
trap 'rm -rf "$preview_dir"' EXIT

mkdir -p "$preview_dir/bin" "$preview_dir/state"

cat > "$preview_dir/bin/curl" <<'SH'
#!/bin/sh
printf '%s\n' '{"tag_name":"v999.0.0","draft":false,"prerelease":false}'
SH

cat > "$preview_dir/bin/herdr" <<'SH'
#!/bin/sh
if [ "${1:-}" = plugin ] && [ "${2:-}" = install ]; then
    sleep 3
    exit 0
fi
if [ -n "${HERDR_PREVIEW_REAL_BIN:-}" ]; then
    exec "$HERDR_PREVIEW_REAL_BIN" "$@"
fi
exit 1
SH

chmod +x "$preview_dir/bin/curl" "$preview_dir/bin/herdr"

cargo build --locked --manifest-path "$project_root/Cargo.toml"

HERDR_PREVIEW_REAL_BIN=${HERDR_BIN_PATH:-$(command -v herdr || true)}
export HERDR_PREVIEW_REAL_BIN
export PATH="$preview_dir/bin:$PATH"
export HERDR_BIN_PATH="$preview_dir/bin/herdr"
export HERDR_PLUGIN_STATE_DIR="$preview_dir/state"
unset HERDR_PLUGIN_ROOT

"$project_root/target/debug/herdr-git"

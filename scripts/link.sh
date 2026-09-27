#!/bin/sh
set -eu

plugin_id=io.github.jehwanyoo.herdr-git.open
config=${HERDR_CONFIG_PATH:-${XDG_CONFIG_HOME:-$HOME/.config}/herdr/config.toml}

if [ -f "$config" ]; then
    binding=$(awk -v plugin_id="$plugin_id" '
        function finish_block() {
            if (in_command && key == "prefix+u") {
                if (type == "plugin_action" && command == plugin_id) found = 1
                else conflict = 1
            }
        }
        /^[[:space:]]*\[/ {
            finish_block()
            in_command = ($0 ~ /^[[:space:]]*\[\[keys\.command\]\][[:space:]]*(#.*)?$/)
            key = type = command = ""
            next
        }
        in_command && /^[[:space:]]*(key|type|command)[[:space:]]*=/ {
            line = $0
            sub(/^[[:space:]]*[^=]+=[[:space:]]*/, "", line)
            if (line !~ /^"[^"]*"/) next
            sub(/^"/, "", line)
            sub(/".*/, "", line)
            if ($0 ~ /^[[:space:]]*key[[:space:]]*=/) key = line
            else if ($0 ~ /^[[:space:]]*type[[:space:]]*=/) type = line
            else command = line
        }
        END {
            finish_block()
            if (conflict) print "conflict"
            else if (found) print "exists"
            else print "missing"
        }
    ' "$config")
else
    binding=missing
fi

if [ "$binding" = conflict ]; then
    echo "prefix+u is already assigned to another command in $config" >&2
    exit 1
fi

herdr plugin link "$PWD"

case "$binding" in
    exists)
        echo "prefix+u already opens Herdr Git"
        ;;
    missing)
        mkdir -p "$(dirname "$config")"
        printf '\n[[keys.command]]\nkey = "prefix+u"\ntype = "plugin_action"\ncommand = "%s"\ndescription = "toggle Git pane"\n' "$plugin_id" >> "$config"
        herdr config check
        herdr server reload-config
        echo "Added prefix+u binding to $config"
        ;;
esac

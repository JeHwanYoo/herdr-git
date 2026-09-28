#!/bin/sh
set -eu

if [ -n "$1" ]; then
    exec "${HERDR_BIN_PATH:-herdr}" plugin install JeHwanYoo/herdr-git --ref "$1" --yes
fi

exec "${HERDR_BIN_PATH:-herdr}" plugin install JeHwanYoo/herdr-git --yes

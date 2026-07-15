#!/usr/bin/env bash
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
    echo "enforce-unprivileged.sh must run as root" >&2
    exit 1
fi

for group in adm cdrom dip lxd sudo; do
    if id -nG shipyard | tr ' ' '\n' | grep -Fxq "$group"; then
        gpasswd -d shipyard "$group"
    fi
done

rm -f /etc/sudoers.d/90-cloud-init-users

actual_groups=$(id -nG shipyard)
if [ "$actual_groups" != "shipyard" ]; then
    echo "shipyard retains forbidden groups: $actual_groups" >&2
    exit 1
fi

install -d -m 0755 /run/shipyard-review
touch /run/shipyard-review/unprivileged-ready

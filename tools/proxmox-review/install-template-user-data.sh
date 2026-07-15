#!/usr/bin/env bash
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
    echo "install-template-user-data.sh must run as root on the Proxmox node" >&2
    exit 1
fi

template_vmid=${1:?usage: install-template-user-data.sh TEMPLATE_VMID USER_DATA_YAML}
user_data=${2:?usage: install-template-user-data.sh TEMPLATE_VMID USER_DATA_YAML}
storage=shipyard-review-iso
snippet_name=shipyard-review-user-data.yaml

test -f "$user_data"
qm config "$template_vmid" | grep -Eq '^template: 1$'
qm status "$template_vmid" | grep -Eq '^status: stopped$'
pvesm set "$storage" --content iso,snippets
install -d -o root -g root -m 0700 /var/lib/vz/shipyard-review-iso/snippets
install -o root -g root -m 0600 "$user_data" \
    "/var/lib/vz/shipyard-review-iso/snippets/$snippet_name"

qm set "$template_vmid" --protection 0
restore_protection() {
    qm set "$template_vmid" --protection 1 >/dev/null
}
trap restore_protection EXIT
qm set "$template_vmid" \
    --cicustom "user=$storage:snippets/$snippet_name"
restore_protection
trap - EXIT

qm config "$template_vmid" | grep -Fq \
    "cicustom: user=$storage:snippets/$snippet_name"
echo "protected explicit cloud-init user-data installed on template $template_vmid"

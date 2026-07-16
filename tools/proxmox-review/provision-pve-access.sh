#!/usr/bin/env bash
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
    echo "provision-pve-access.sh must run as root on the Proxmox node" >&2
    exit 1
fi

template_vmid=${1:-132}
job_pool=shipyard-review-jobs
iso_storage=shipyard-review-iso
identity=shipyard-review@pve

qm config "$template_vmid" | grep -Eq '^template: 1$'
qm config "$template_vmid" | grep -Eq '^protection: 1$'
if qm config "$template_vmid" | grep -Eq '^(net[0-9]+|ipconfig[0-9]+):'; then
    echo "protected review template must not have a network device or IP configuration" >&2
    exit 1
fi

install -d -o root -g root -m 0700 /var/lib/vz/shipyard-review-iso
pvesm status | awk 'NR > 1 {print $1}' | grep -Fxq "$iso_storage" || \
    pvesm add dir "$iso_storage" \
        --path /var/lib/vz/shipyard-review-iso \
        --content iso,snippets --nodes "$(hostname)" --shared 0
pvesm set "$iso_storage" --content iso,snippets
install -d -o root -g root -m 0700 /var/lib/vz/shipyard-review-iso/snippets

pveum pool list --output-format json | jq -e \
    --arg pool "$job_pool" '.[] | select(.poolid == $pool)' >/dev/null || \
    pveum pool add "$job_pool" \
        --comment "Disposable untrusted Shipyard review VMs only"

upsert_role() {
    local role=$1
    local privileges=$2
    if pveum role list --output-format json | jq -e \
        --arg role "$role" '.[] | select(.roleid == $role)' >/dev/null; then
        pveum role modify "$role" --privs "$privileges"
    else
        pveum role add "$role" --privs "$privileges"
    fi
}

upsert_role ShipyardReviewClone \
    "VM.Audit VM.Clone"
upsert_role ShipyardReviewFleetAudit \
    "VM.Audit"
upsert_role ShipyardReviewJob \
    "Pool.Audit VM.Allocate VM.Audit VM.Config.CDROM VM.Config.Cloudinit VM.Config.Network VM.Config.Options VM.GuestAgent.Audit VM.GuestAgent.FileRead VM.GuestAgent.Unrestricted VM.PowerMgmt"
upsert_role ShipyardReviewDisk \
    "Datastore.AllocateSpace Datastore.Audit"
upsert_role ShipyardReviewStorage \
    "Datastore.Allocate Datastore.AllocateSpace Datastore.AllocateTemplate Datastore.Audit"

pveum user list --output-format json | jq -e \
    --arg user "$identity" '.[] | select(.userid == $user)' >/dev/null || \
    pveum user add "$identity" \
        --comment "Credential-free identity for disposable review coordinator"

# Revoke the obsolete bridge grant from bridge-based templates. Image
# maintenance is a separate root-owned operation; the controller gets no SDN
# capability.
if pveum acl list --output-format json | jq -e --arg identity "$identity" \
    '.[] | select(.path == "/sdn/zones/localnetwork/vmbr1" and .ugid == $identity and .roleid == "ShipyardReviewBridge")' \
    >/dev/null; then
    pveum acl delete /sdn/zones/localnetwork/vmbr1 \
        --users "$identity" --roles ShipyardReviewBridge
fi

pveum acl modify "/vms/$template_vmid" \
    --users "$identity" --roles ShipyardReviewClone
pveum acl modify /vms \
    --users "$identity" --roles ShipyardReviewFleetAudit
pveum acl modify "/pool/$job_pool" \
    --users "$identity" --roles ShipyardReviewJob
pveum acl modify /storage/local-lvm \
    --users "$identity" --roles ShipyardReviewDisk
pveum acl modify "/storage/$iso_storage" \
    --users "$identity" --roles ShipyardReviewStorage

echo "PVE identity and ACLs installed; no API token was created."

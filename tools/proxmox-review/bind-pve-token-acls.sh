#!/usr/bin/env bash
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
    echo "bind-pve-token-acls.sh must run as root on the Proxmox node" >&2
    exit 1
fi

token_id=${1:?usage: bind-pve-token-acls.sh TOKEN_ID [TEMPLATE_VMID]}
template_vmid=${2:-124}
case "$token_id" in
    *[!A-Za-z0-9._-]*|'') echo "invalid token id" >&2; exit 1 ;;
esac
token="shipyard-review@pve!$token_id"

pveum user token list shipyard-review@pve --output-format json | jq -e \
    --arg id "$token_id" '.[] | select(.tokenid == $id)' >/dev/null

pveum acl modify "/vms/$template_vmid" \
    --tokens "$token" --roles ShipyardReviewClone
pveum acl modify /pool/shipyard-review-jobs \
    --tokens "$token" --roles ShipyardReviewJob
pveum acl modify /storage/local-lvm \
    --tokens "$token" --roles ShipyardReviewDisk
pveum acl modify /storage/shipyard-review-iso \
    --tokens "$token" --roles ShipyardReviewStorage
pveum acl modify /sdn/zones/localnetwork/vmbr1 \
    --tokens "$token" --roles ShipyardReviewBridge

#!/usr/bin/env bash
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
    echo "harden-control.sh must run as root" >&2
    exit 1
fi

asset_dir=${1:-/tmp/shipyard-control-hardening}
export DEBIAN_FRONTEND=noninteractive

apt-get update
apt-get install -y --no-install-recommends \
    ca-certificates curl git jq python3 python3-venv qemu-guest-agent ufw

install -m 0644 \
    "$asset_dir/99-shipyard-control-sshd.conf" \
    /etc/ssh/sshd_config.d/99-shipyard-control.conf
install -m 0644 \
    "$asset_dir/99-shipyard-control-sysctl.conf" \
    /etc/sysctl.d/99-shipyard-control.conf
sshd -t
sysctl --system >/dev/null

if id -nG shipyard-control | tr ' ' '\n' | grep -Fxq lxd; then
    gpasswd -d shipyard-control lxd
fi

install -d -m 0700 /etc/shipyard-review /var/lib/shipyard-review

ufw --force reset
ufw default deny incoming
ufw default deny outgoing
ufw default deny routed

# Administrative SSH is reachable only through the Proxmox host.
ufw allow in from 192.168.86.70 to any port 22 proto tcp

# Narrow control-plane egress. Specific private destinations precede the
# private-range denials; public traffic is limited to HTTP(S).
ufw allow out to 192.168.86.1 port 53 proto udp
ufw allow out to 192.168.86.1 port 53 proto tcp
ufw allow out to 192.168.86.1 port 123 proto udp
ufw allow out to 192.168.86.70 port 8006 proto tcp
ufw deny out to 10.0.0.0/8
ufw deny out to 172.16.0.0/12
ufw deny out to 192.168.0.0/16
ufw deny out to 169.254.0.0/16
ufw allow out 80/tcp
ufw allow out 443/tcp

ufw logging low
ufw --force enable

systemctl restart qemu-guest-agent.service

apt-get clean
rm -rf /var/lib/apt/lists/* "$asset_dir"

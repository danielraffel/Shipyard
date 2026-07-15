#!/usr/bin/env bash
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
    echo "harden-template.sh must run as root" >&2
    exit 1
fi

asset_dir=${1:-/tmp/shipyard-review-hardening}

install -m 0644 \
    "$asset_dir/99-shipyard-review-cloud.cfg" \
    /etc/cloud/cloud.cfg.d/99-shipyard-review.cfg
install -m 0644 \
    "$asset_dir/99-shipyard-review-sshd.conf" \
    /etc/ssh/sshd_config.d/99-shipyard-review.conf
install -m 0644 \
    "$asset_dir/99-shipyard-review-sysctl.conf" \
    /etc/sysctl.d/99-shipyard-review.conf
systemctl disable --now systemd-resolved.service || true
rm -f /etc/resolv.conf
install -m 0644 "$asset_dir/no-network-resolv.conf" /etc/resolv.conf
install -m 0755 \
    "$asset_dir/enforce-unprivileged.sh" \
    /usr/local/sbin/shipyard-enforce-unprivileged
install -m 0644 \
    "$asset_dir/shipyard-review-guest-hardening.service" \
    /etc/systemd/system/shipyard-review-guest-hardening.service

if grep -q '^    name: ubuntu$' /etc/cloud/cloud.cfg; then
    patch --forward --batch /etc/cloud/cloud.cfg \
        "$asset_dir/cloud-default-user.patch"
elif ! grep -q '^    name: shipyard$' /etc/cloud/cloud.cfg; then
    echo "cloud-init default user is neither ubuntu nor shipyard" >&2
    exit 1
fi
systemctl enable shipyard-review-guest-hardening.service

sshd -t
sysctl --system >/dev/null

# Ubuntu cloud images normally grant the default account passwordless sudo and
# membership in lxd. Both are root-equivalent and forbidden in a job guest.
for group in adm cdrom dip lxd sudo; do
    if id -nG shipyard | tr ' ' '\n' | grep -Fxq "$group"; then
        gpasswd -d shipyard "$group"
    fi
done
rm -f /etc/sudoers.d/90-cloud-init-users
passwd -l root >/dev/null

# Each linked clone receives a new one-run SSH key and instance identity.
rm -f /root/.ssh/authorized_keys /home/shipyard/.ssh/authorized_keys
cloud-init clean --logs --machine-id --seed
rm -rf "$asset_dir"

apt-get clean
rm -rf /var/lib/apt/lists/* /tmp/* /var/tmp/*
fstrim -av || true

sync
shutdown -h now

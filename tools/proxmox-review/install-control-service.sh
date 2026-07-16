#!/usr/bin/env bash
set -euo pipefail

if [ "$(id -u)" -ne 0 ]; then
    echo "install-control-service.sh must run as root" >&2
    exit 1
fi

asset_dir=${1:-/tmp/shipyard-review-control}

if ! getent group shipyard-review >/dev/null; then
    groupadd --system shipyard-review
fi
if ! id shipyard-review >/dev/null 2>&1; then
    useradd --system --gid shipyard-review --home-dir /var/lib/shipyard-review \
        --shell /usr/sbin/nologin shipyard-review
fi

apt-get update
DEBIAN_FRONTEND=noninteractive apt-get install -y --no-install-recommends \
    gh openssl python3 xorriso

install -d -o root -g shipyard-review -m 0750 /etc/shipyard-review
install -d -o root -g shipyard-review -m 0750 /etc/shipyard-review/recipes
install -d -o root -g shipyard-review -m 0750 \
    /etc/shipyard-review/dependencies \
    /etc/shipyard-review/images
install -d -o root -g root -m 0755 /usr/local/libexec/shipyard-review
install -d -o shipyard-review -g shipyard-review -m 0700 \
    /var/lib/shipyard-review \
    /var/lib/shipyard-review/results \
    /var/lib/shipyard-review/secrets
install -m 0755 "$asset_dir/review-control.py" \
    /usr/local/libexec/shipyard-review/review-control.py
install -m 0755 "$asset_dir/comment-poller.py" \
    /usr/local/libexec/shipyard-review/comment-poller.py
install -m 0755 "$asset_dir/github-app-token.py" \
    /usr/local/libexec/shipyard-review/github-app-token.py
install -m 0755 "$asset_dir/guest-runner.py" \
    /usr/local/libexec/shipyard-review/guest-runner.py
install -m 0755 "$asset_dir/verify-dependency-inventory.py" \
    /usr/local/libexec/shipyard-review/verify-dependency-inventory.py
install -m 0644 "$asset_dir/test_review_control.py" \
    /usr/local/libexec/shipyard-review/test_review_control.py
install -m 0644 "$asset_dir/comment-policy.example.json" \
    /usr/local/libexec/shipyard-review/comment-policy.example.json
install -m 0755 "$asset_dir/ghapp-control" /usr/local/bin/ghapp
install -o root -g shipyard-review -m 0640 "$asset_dir/recipes/pulp-linux.json" \
    /etc/shipyard-review/recipes/pulp-linux.json
install -o root -g shipyard-review -m 0640 "$asset_dir/recipes/pulp-signal.json" \
    /etc/shipyard-review/recipes/pulp-signal.json
install -o root -g shipyard-review -m 0640 "$asset_dir/recipes/pulp-fft.json" \
    /etc/shipyard-review/recipes/pulp-fft.json
install -o root -g shipyard-review -m 0640 "$asset_dir/dependencies/pulp-linux.json" \
    /etc/shipyard-review/dependencies/pulp-linux.json
install -o root -g shipyard-review -m 0640 "$asset_dir/images/shipyard-review-template-v9.json" \
    /etc/shipyard-review/images/shipyard-review-template-v9.json
install -o root -g shipyard-review -m 0640 "$asset_dir/images/shipyard-review-template-v10.json" \
    /etc/shipyard-review/images/shipyard-review-template-v10.json
install -m 0644 "$asset_dir/shipyard-review-poll.service" \
    /etc/systemd/system/shipyard-review-poll.service
install -m 0644 "$asset_dir/shipyard-review-poll.timer" \
    /etc/systemd/system/shipyard-review-poll.timer

python3 -m py_compile \
    /usr/local/libexec/shipyard-review/review-control.py \
    /usr/local/libexec/shipyard-review/comment-poller.py \
    /usr/local/libexec/shipyard-review/guest-runner.py \
    /usr/local/libexec/shipyard-review/verify-dependency-inventory.py \
    /usr/local/libexec/shipyard-review/test_review_control.py
systemd-analyze verify /etc/systemd/system/shipyard-review-poll.service
systemctl daemon-reload

# Enabling is a separate, explicit step after both policies are changed from
# enabled=false and all live credentials have a verified backup.
systemctl disable --now shipyard-review-poll.timer >/dev/null 2>&1 || true

apt-get clean
rm -rf /var/lib/apt/lists/*

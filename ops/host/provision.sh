#!/usr/bin/env bash
# One-time setup of the co-located shadow's host (Task 8.5 R25): Ubuntu 24.04
# on EC2, run as root through SSM. Idempotent. See ops/host/README.md.
set -euo pipefail
[[ $EUID -eq 0 ]] || { echo "provision: run as root" >&2; exit 1; }
export DEBIAN_FRONTEND=noninteractive
apt-get update -y
apt-get install -y build-essential pkg-config git chrony jq curl ca-certificates
# Amazon Time Sync: every ledger timestamp is this clock's.
grep -q 169.254.169.123 /etc/chrony/chrony.conf || sed -i '1i server 169.254.169.123 prefer iburst minpoll 4 maxpoll 4' /etc/chrony/chrony.conf
systemctl restart chrony
# 4 GB of swap: the release build's fat-LTO link step.
if ! swapon --show | grep -q /swapfile; then
  fallocate -l 4G /swapfile && chmod 600 /swapfile && mkswap /swapfile && swapon /swapfile
  echo '/swapfile none swap sw 0 0' >> /etc/fstab
fi
id apex >/dev/null 2>&1 || useradd --system --create-home --home-dir /home/apex --shell /usr/sbin/nologin apex
install -d -o apex -g apex /opt/apex /opt/apex/var /opt/apex/var/apex
# R36: armed-live's keys, ledger, targets and kill file: the service's own,
# and outside var/apex, which a new revision archives.
install -d -o apex -g apex -m 0700 /opt/apex/var/armed
# The binary is root's: the service user runs it and cannot replace it.
install -d -o root -g root -m 0755 /opt/apex/bin
install -d -o root -g root -m 0700 /etc/apex
# The toolchain rust-toolchain.toml pins, for the apex user.
if [[ ! -x /home/apex/.cargo/bin/cargo ]]; then
  sudo -u apex -H bash -c 'curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.97.0 --profile minimal'
fi
echo "provisioned: $(chronyc tracking | grep -E 'Reference ID|System time')"

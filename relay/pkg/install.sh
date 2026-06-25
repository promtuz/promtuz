#!/bin/sh
# Promtuz relay repo installer — adds the apt repo (key + source) and installs
# pzrelay. Debian / Ubuntu only.
#
#   curl -fsSL https://apt.promtuz.dev/install.sh | sudo sh
#   curl -fsSL https://apt.promtuz.dev/install.sh | sudo CHANNEL=edge sh

set -e

CHANNEL="${CHANNEL:-edge}"
BASE="https://apt.promtuz.dev"
KEYRING="/etc/apt/keyrings/promtuz.asc"
LIST="/etc/apt/sources.list.d/promtuz.list"

if [ "$(id -u)" -ne 0 ]; then
    echo "error: run as root (e.g. pipe to 'sudo sh')." >&2
    exit 1
fi

if ! command -v apt-get >/dev/null 2>&1; then
    echo "error: this repo is Debian/Ubuntu (apt) only." >&2
    exit 1
fi

# Fetch tools for an https download, only if missing.
if ! command -v curl >/dev/null 2>&1; then
    apt-get update -qq
    apt-get install -y -qq ca-certificates curl
fi

# Trust anchor: your repo's public signing key.
install -d -m 0755 /etc/apt/keyrings
curl -fsSL "$BASE/promtuz-archive-keyring.asc" -o "$KEYRING"

# Source list, pinned to the keyring so only this key is trusted for this repo.
echo "deb [signed-by=$KEYRING] $BASE $CHANNEL main" > "$LIST"

apt-get update
apt-get install -y pzrelay

cat <<EOF

pzrelay installed from the '$CHANNEL' channel.
Next: edit /etc/pzrelay/relay.toml, provision certs, then
      systemctl enable --now pzrelay
Updates later: apt update && apt upgrade   (your config is preserved)
EOF

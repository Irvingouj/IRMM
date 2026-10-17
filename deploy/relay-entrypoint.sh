#!/bin/sh
# Writes the iroh-relay config from the environment, then runs the relay.
# Access is decided by the IRMM server (/api/relay/access); the bearer token comes
# from IROH_RELAY_HTTP_BEARER_TOKEN.
set -eu
: "${IRMM_RELAY_HOSTNAME:?set IRMM_RELAY_HOSTNAME}"
: "${IRMM_ACME_EMAIL:?set IRMM_ACME_EMAIL}"
: "${IRMM_SERVER_INTERNAL_URL:=http://server:8080}"
cat > /etc/iroh-relay.toml <<CONF
enable_relay = true
http_bind_addr = "[::]:80"
enable_quic_addr_discovery = true

[tls]
cert_mode = "LetsEncrypt"
hostname = "${IRMM_RELAY_HOSTNAME}"
contact = "${IRMM_ACME_EMAIL}"
cert_dir = "/certs"

[access.http]
url = "${IRMM_SERVER_INTERNAL_URL}/api/relay/access"
CONF
exec iroh-relay --config-path /etc/iroh-relay.toml

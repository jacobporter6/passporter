#!/usr/bin/env bash
# One-time setup of passporter-bridge for the user who runs herdr.
#
#   deploy/bridge/setup.sh [options]     # from anywhere inside the repo checkout
#
# Options:
#   --listen ADDR:PORT    where the bridge listens (default 127.0.0.1:8443).
#                         Keep it on loopback and put an HTTPS proxy in front
#                         (tailscale serve, Caddy): see docs/deploy.md.
#   --trusted-proxy IP    the HTTPS proxy's address, the only peer whose
#                         X-Forwarded-For is believed (default 127.0.0.1 when
#                         --listen is loopback, else none).
#   --no-service          skip the systemd user unit (start passporter-bridge
#                         yourself, or under your own supervisor).
#   --config-only         only write the config (step 3): no build, no
#                         install, no unit.
#   --insecure-mvp IP     NOT RECOMMENDED. Plain HTTP on IP:8443 plus a static
#                         bearer token, for a phone that reaches this machine
#                         only over a WireGuard tunnel and cannot use HTTPS.
#                         Read "Insecure MVP mode" in docs/deploy.md first.
#   -h, --help            show this message.
#
# What it does:
#   1. builds the bridge (release) and installs passporter-bridge and the
#      passporter CLI (pair, devices, doctor) to ~/.local/bin;
#   2. builds the PWA into crates/web/dist if trunk is available;
#   3. creates ~/.config/passporter (0700) and config.toml (0600). The
#      default config is pairing-only: no static token, no plain HTTP off
#      loopback. An existing config.toml is never touched;
#   4. installs, enables and (re)starts the systemd user unit, unless
#      --no-service.
#
# Safe to re-run: later runs rebuild, reinstall and restart, keeping the config.
set -euo pipefail

usage() { sed -n '2,/^set -euo/p' "${BASH_SOURCE[0]}" | sed -e '$d' -e 's/^# \{0,1\}//'; }

listen="127.0.0.1:8443"
trusted_proxy=""
trusted_proxy_set=0
service=1
build=1
mvp_ip=""

while [[ $# -gt 0 ]]; do
    case "$1" in
        --listen) listen="${2:?--listen needs ADDR:PORT}"; shift 2 ;;
        --trusted-proxy) trusted_proxy="${2:?--trusted-proxy needs an IP}"; trusted_proxy_set=1; shift 2 ;;
        --no-service) service=0; shift ;;
        --config-only) build=0; service=0; shift ;;
        --insecure-mvp) mvp_ip="${2:?--insecure-mvp needs the tunnel-reachable IP of this machine}"; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) echo "unknown option: $1 (see --help)" >&2; exit 2 ;;
    esac
done

if [[ -n "$mvp_ip" && ( "$listen" != "127.0.0.1:8443" || $trusted_proxy_set -eq 1 ) ]]; then
    echo "--insecure-mvp sets its own listen address and cannot be used with a trusted proxy" >&2
    exit 2
fi
if [[ $trusted_proxy_set -eq 0 && "$listen" == 127.* ]]; then
    trusted_proxy="127.0.0.1"
fi

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
config_dir="${HOME}/.config/passporter"
config="${config_dir}/config.toml"
state_dir="${HOME}/.local/state/passporter"
bin_dir="${HOME}/.local/bin"
unit_dir="${HOME}/.config/systemd/user"
dist="${repo}/crates/web/dist"

say() { printf '\n==> %s\n' "$*"; }

if [[ $build -eq 1 ]]; then
    say "Building passporter-bridge (release)"
    (cd "$repo" && cargo build --release --locked -p bridge)
    install -d -m 0755 "$bin_dir"
    for bin in passporter-bridge passporter; do
        install -m 0755 "${repo}/target/release/${bin}" "${bin_dir}/${bin}.new"
        mv -f "${bin_dir}/${bin}.new" "${bin_dir}/${bin}"
    done

    if command -v trunk >/dev/null 2>&1; then
        say "Building the PWA into ${dist}"
        (cd "${repo}/crates/web" && trunk build --release --locked)
    else
        say "trunk not found: skipping the PWA build (run 'just web-build' later)"
    fi
fi

say "Config in ${config_dir}"
install -d -m 0700 "$config_dir"
chmod 0700 "$config_dir"
install -d -m 0700 "$state_dir"
if [[ -e "$config" ]]; then
    echo "Keeping the existing ${config}."
else
    tmp="$(mktemp "${config_dir}/config.toml.XXXXXX")"
    chmod 0600 "$tmp"
    if [[ -n "$mvp_ip" ]]; then
        token="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
        if [[ ${#token} -ne 64 ]]; then
            echo "token generation failed" >&2
            exit 1
        fi
        cat >"$tmp" <<EOF
# passporter-bridge config: INSECURE MVP MODE (docs/deploy.md).
# Plain HTTP and a permanent static token. Only for a phone that reaches
# this machine solely over WireGuard. Switch to HTTPS and pairing soon.
listen = "${mvp_ip}:8443"
insecure_http_over_wireguard = true
insecure_http_allow = ["${mvp_ip}"]
api_token = "${token}"
allow_static_token = true
static_dir = "${dist}"
state_dir = "${state_dir}"
EOF
    else
        {
            echo "# passporter-bridge config (docs/deploy.md). Pairing only: run 'passporter pair'."
            echo "listen = \"${listen}\""
            if [[ -n "$trusted_proxy" ]]; then
                echo "trusted_proxy = \"${trusted_proxy}\""
            fi
            echo "static_dir = \"${dist}\""
            echo "state_dir = \"${state_dir}\""
        } >"$tmp"
    fi
    mv "$tmp" "$config"
    if [[ -n "$mvp_ip" ]]; then
        cat <<EOF

  !!! INSECURE MVP MODE !!!
  The bridge serves plain HTTP on ${mvp_ip}:8443 and accepts a permanent,
  unrevocable bearer token. Anyone who can reach that port and sniff or
  guess the token can type into your agents. Firewall the port to your
  WireGuard subnet only, and move to HTTPS + pairing (docs/deploy.md).

  Your bridge token (shown only now; it is stored in ${config}):

      ${token}

  Open http://${mvp_ip}:8443/ on the phone (tunnel on) and paste it.
EOF
    fi
fi
chmod 0600 "$config"

if [[ $build -eq 0 ]]; then
    say "Config written; nothing built or installed (--config-only)"
    exit 0
elif [[ $service -eq 1 ]]; then
    say "Installing the systemd user unit"
    install -d -m 0755 "$unit_dir"
    install -m 0644 "${repo}/deploy/bridge/passporter-bridge.service" "${unit_dir}/passporter-bridge.service"
    systemctl --user daemon-reload
    systemctl --user enable passporter-bridge.service
    systemctl --user restart passporter-bridge.service
    sleep 1
    systemctl --user --no-pager --lines=15 status passporter-bridge.service || true
else
    say "Skipping the systemd unit: start the bridge with"
    echo "  ${bin_dir}/passporter-bridge --config-dir ${config_dir}"
fi

cat <<EOF

Done. Next steps (docs/deploy.md has the details):
  tailscale serve --bg --https=443 http://${listen}   # or Caddy: HTTPS in front
  passporter pair                                    # QR + code for the phone
  passporter doctor                                  # check the deployment (read-only)

Useful commands:
  journalctl --user -u passporter-bridge -f      # logs
  systemctl --user restart passporter-bridge     # after rebuilding
  loginctl enable-linger "\$USER"                 # keep it running while logged out
  tail -f ${state_dir}/audit.jsonl               # every action the phone sent
EOF

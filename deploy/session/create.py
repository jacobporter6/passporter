#!/usr/bin/env python3
"""Generate an isolated one-session bridge + private ntfy deployment. Never starts services.

Writes a new directory with a bridge config for one explicit herdr socket, a
Docker Compose file for the built-in Android transport's companion (ntfy,
pinned by digest, loopback only, read-only, no capabilities) and run.sh,
which starts both and stops the companion when the bridge exits. See
docs/operations/push.md ("Built-in delivery") and docs/deploy.md.
"""

import argparse
import json
import hashlib
import os
from pathlib import Path
import re
import shlex

NTFY_IMAGE = "binwiederhier/ntfy@sha256:1875ddcf815c8a3c981d6d78c67a93452d0abba79b1ab9520b1f5f5238902bd7"  # v2.37.2


def port(value):
    number = int(value)
    if not 1024 <= number <= 65535:
        raise argparse.ArgumentTypeError("choose a non-privileged port from 1024 to 65535")
    return number


def create(args):
    if not re.fullmatch(r"[a-z][a-z0-9-]{0,31}", args.name):
        raise ValueError("name must be 1-32 lowercase letters, digits or hyphens, starting with a letter")
    if not re.fullmatch(r"(mailto:|https://)[^\s\"\\]+", args.contact):
        raise ValueError("contact must be a mailto: or https:// URL (the VAPID contact push services see)")
    if args.bridge_port == args.ntfy_port:
        raise ValueError("bridge and ntfy need different ports")
    root = args.output.expanduser().absolute()
    binary = args.bridge_bin.expanduser().resolve(strict=True)
    static = args.static_dir.expanduser().resolve(strict=True)
    socket = args.herdr_socket.expanduser().absolute()
    if not args.herdr_socket.expanduser().is_absolute():
        raise ValueError("herdr socket must be explicit and absolute (including for the default session)")
    if not binary.is_file() or not os.access(binary, os.X_OK):
        raise ValueError("bridge binary must be executable")
    if not (static / "index.html").is_file():
        raise ValueError("static directory needs the built PWA (just web-build)")
    for value in (str(root), str(binary), str(static), str(socket)):
        if any(c in value for c in "\r\n\x00"):
            raise ValueError("paths cannot contain control characters")
    # Unix socket length includes the trailing NUL. This path is also the hook forwarder's setting.
    hook = root / "hooks" / "hooks.sock"
    if len(os.fsencode(hook)) > 103:
        raise ValueError("output path is too long for the per-session hook socket")
    old_mask = os.umask(0o077)
    try:
        root.mkdir(parents=True, exist_ok=False)
        for name in ("state", "ntfy-cache", "hooks"):
            (root / name).mkdir(mode=0o700)
        quote = json.dumps  # TOML basic strings and YAML quoted paths share these escapes.
        config = f'''# One bridge, one explicit herdr session. Generated; HTTPS belongs at your edge.
listen = "127.0.0.1:{args.bridge_port}"
trusted_proxy = "127.0.0.1"
herdr_socket = {quote(str(socket))}
static_dir = {quote(str(static))}
state_dir = {quote(str(root / 'state'))}
audit_log = {quote(str(root / 'state' / 'audit.jsonl'))}

[push]
subject = {quote(args.contact)}
native_ntfy = "127.0.0.1:{args.ntfy_port}"
'''
        (root / "config.toml").write_text(config)
        project = "passporter-" + args.name + "-" + hashlib.sha256(str(root).encode()).hexdigest()[:12]
        compose = f'''name: {project}
services:
  notifications:
    image: {NTFY_IMAGE}
    network_mode: host
    user: "{os.getuid()}:{os.getgid()}"
    read_only: true
    cap_drop: [ALL]
    security_opt: [no-new-privileges:true]
    restart: unless-stopped
    volumes:
      - type: bind
        source: {quote(str(root / 'ntfy-cache'))}
        target: /var/cache/ntfy
    command:
      - serve
      - --listen-http=127.0.0.1:{args.ntfy_port}
      - --cache-file=/var/cache/ntfy/cache.db
      - --cache-duration=1h
      - --keepalive-interval=20s
    healthcheck:
      test: [CMD, wget, -q, -O, /dev/null, "http://127.0.0.1:{args.ntfy_port}/v1/health"]
      interval: 10s
      timeout: 3s
      retries: 3
'''
        (root / "compose.yml").write_text(compose)
        command = f"docker compose -f {shlex.quote(str(root / 'compose.yml'))}"
        run = f'''#!/usr/bin/env bash
set -euo pipefail
umask 077
bridge_pid=""
cleanup() {{
    trap - EXIT INT TERM
    if [[ -n "$bridge_pid" ]]; then
        kill -TERM "$bridge_pid" 2>/dev/null || true
        wait "$bridge_pid" 2>/dev/null || true
    fi
    {command} down
}}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
if [[ ! -e {shlex.quote(str(root / 'vapid.pem'))} ]]; then
    {shlex.quote(str(binary))} push-keygen --config-dir {shlex.quote(str(root))} >/dev/null
fi
{command} up --detach --wait --wait-timeout 45
export PASSPORTER_HOOK_SOCKET={shlex.quote(str(hook))}
{shlex.quote(str(binary))} --config-dir {shlex.quote(str(root))} &
bridge_pid=$!
wait "$bridge_pid"
'''
        (root / "run.sh").write_text(run)
        (root / "run.sh").chmod(0o700)
        (root / "README.txt").write_text(f'''Session deployment: {args.name}
Start both services: {root / 'run.sh'}
Proxy your chosen HTTPS bridge name to 127.0.0.1:{args.bridge_port}.
Never proxy or expose ntfy on 127.0.0.1:{args.ntfy_port}; only the bridge may access it.
For hooks, set PASSPORTER_HOOK_SOCKET={hook} in agents of this herdr session.
Pair independently using passporter --config-dir {root} pair.
Stop run.sh to stop this deployment only. State and cache are retained.
To keep it running, supervise run.sh with your user service manager.
Offline installation: preload {NTFY_IMAGE}; no cloud is used for message delivery.
Do not run a copy of this directory beside the original: its device records and keys must stay unique.
''')
    finally:
        os.umask(old_mask)
    return root


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--name", required=True, help="unique deployment label (including default)")
    parser.add_argument("--output", required=True, type=Path, help="new deployment directory; never overwritten")
    parser.add_argument("--herdr-socket", required=True, type=Path)
    parser.add_argument("--bridge-bin", required=True, type=Path)
    parser.add_argument("--static-dir", required=True, type=Path)
    parser.add_argument("--contact", required=True, help="VAPID contact: mailto:you@example.com or an https:// URL")
    parser.add_argument("--bridge-port", type=port, default=8443)
    parser.add_argument("--ntfy-port", type=port, default=2586)
    args = parser.parse_args()
    try:
        print(create(args))
    except (ValueError, OSError) as error:
        parser.error(str(error))


if __name__ == "__main__":
    main()

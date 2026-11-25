#!/usr/bin/env bash
# rustadsb installer: download a release, check it, and run it as a systemd service.
#
#   curl -fsSL https://raw.githubusercontent.com/phillip-bailey/openradome/main/install.sh | sudo bash -s -- --lat 51.5 --lon -0.1
#   sudo ./install.sh --lat 51.5 --lon -0.1              # from a checkout
#   sudo ./install.sh --archive rustadsb-v0.5.0-linux-armv7.tar.gz --lat ... --lon ...
#   sudo ./install.sh --rollback                         # back to the previous binary
#   sudo ./install.sh --uninstall [--purge]
#
# What it does, on Debian, Raspberry Pi OS, Ubuntu or any Linux with systemd:
#   * picks the build for this CPU (x86_64, arm64, armv7, armv6), downloads it and its SHA256SUMS from
#     the release page and refuses to install if the checksum does not match;
#   * installs /usr/local/bin/rustadsb (the previous one is kept as rustadsb.prev);
#   * creates the system user rustadsb, in plugdev, and a udev rule that gives plugdev the RTL-SDR dongle;
#   * keeps the kernel's DVB-T driver off the dongle (it would claim it first);
#   * writes /etc/rustadsb/rustadsb.toml (only on the first install: your edits are kept on upgrade) and
#     /etc/systemd/system/rustadsb.service, then starts it (installs from before 0.4 keep their
#     /etc/default/rustadsb until --migrate-config converts it);
#   * after an upgrade, restarts any other service still running the old binary (feed clients that run
#     rustadsb --net-only, for example).
# Re-running the script upgrades the binary and keeps the configuration.
set -euo pipefail

# The repository releases are downloaded from (any GitHub or Gitea repository URL works).
DEFAULT_REPO_URL="https://github.com/phillip-bailey/openradome"
REPO_URL="${RUSTADSB_REPO_URL:-$DEFAULT_REPO_URL}"
VERSION="${RUSTADSB_VERSION:-latest}"
PREFIX=/usr/local/bin
BIN="$PREFIX/rustadsb"
CONF=/etc/rustadsb/rustadsb.toml
LEGACY=/etc/default/rustadsb   # the options file before 0.4 (shell variables), still read until migrated
MCP_TOKEN=/etc/rustadsb/mcp-token
UNIT=/etc/systemd/system/rustadsb.service
UDEV=/etc/udev/rules.d/60-rustadsb-rtlsdr.rules
BLACKLIST=/etc/modprobe.d/rustadsb-blacklist-dvb.conf
USER_NAME=rustadsb

LAT="" LON="" GAIN="49.6" JSON_DIR="/run/rustadsb" BIND="0.0.0.0" ARCHIVE="" REPLACE=0 UNINSTALL=0 PURGE=0 START=1
ROLLBACK=0 TOKEN="${RUSTADSB_TOKEN:-}" DASHBOARD_PORT="" MIGRATE=0 MCP_PORT=""

usage() {
  sed -n '2,20p' "$0" 2>/dev/null | sed 's/^# \{0,1\}//'
  cat <<EOF
Options:
  --lat DEG --lon DEG   receiver position (needed for positions from single messages and the range outline)
  --gain DB             tuner gain (default 49.6, the maximum)
  --version vX.Y.Z      release to install (default: the latest)
  --archive FILE        install from a downloaded release archive instead (checked against SHA256SUMS
                        next to it if there is one)
  --json-dir DIR        where aircraft.json goes (default /run/rustadsb, directly under /run)
  --bind ADDR           address the network ports listen on (default 0.0.0.0; 127.0.0.1 = this host only)
  --dashboard-port PORT serve the status web page on PORT (off by default; see the README)
  --migrate-config      convert an older /etc/default/rustadsb into /etc/rustadsb/rustadsb.toml
  --mcp-port PORT       serve the MCP server on PORT, with a new random token in /etc/rustadsb/mcp-token
  --replace             stop and disable dump1090 if it is running (it holds the dongle)
  --no-start            install everything but do not start the service
  --rollback            put the previous binary (rustadsb.prev) back and restart
  --uninstall [--purge] remove the service and binary (--purge also removes the configuration and user)
Environment: RUSTADSB_REPO_URL (default $REPO_URL), RUSTADSB_VERSION, RUSTADSB_TOKEN (for a private
Gitea release page: sent as an Authorization header). From a private GitHub repository, download the
archive and SHA256SUMS with `gh release download` and use --archive.
EOF
}

die() { echo "install.sh: $*" >&2; exit 1; }
say() { echo "==> $*"; }

while [ $# -gt 0 ]; do
  case "$1" in
    --lat) LAT="$2"; shift 2 ;;
    --lon) LON="$2"; shift 2 ;;
    --gain) GAIN="$2"; shift 2 ;;
    --version) VERSION="$2"; shift 2 ;;
    --archive) ARCHIVE="$2"; shift 2 ;;
    --json-dir) JSON_DIR="$2"; shift 2 ;;
    --bind) BIND="$2"; shift 2 ;;
    --dashboard-port) DASHBOARD_PORT="$2"; shift 2 ;;
    --migrate-config) MIGRATE=1; shift ;;
    --mcp-port) MCP_PORT="$2"; shift 2 ;;
    --replace) REPLACE=1; shift ;;
    --no-start) START=0; shift ;;
    --rollback) ROLLBACK=1; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    --purge) PURGE=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) die "unknown option $1 (see --help)" ;;
  esac
done

[ "$(id -u)" -eq 0 ] || die "run as root (sudo)"
[ "$(uname -s)" = Linux ] || die "this installer is for Linux; see the README for other systems"
HAVE_SYSTEMD=0
[ -d /run/systemd/system ] && HAVE_SYSTEMD=1

# Services other than rustadsb.service whose rustadsb process still runs a replaced (deleted) binary.
stale_units() {
  local pid
  for pid in $(pgrep -x rustadsb 2>/dev/null); do
    case "$(readlink "/proc/$pid/exe" 2>/dev/null)" in
      *"(deleted)") sed -n 's|.*/\([^/]*\.service\)$|\1|p' "/proc/$pid/cgroup" ;;
    esac
  done | sort -u | { grep -vx rustadsb.service || true; }
}

restart_stale() {
  local units
  units=$(stale_units)
  [ -n "$units" ] || return 0
  # shellcheck disable=SC2086
  systemctl restart $units
  say "restarted $(echo "$units" | wc -w) other service(s) still on the old binary"
}

if [ "$ROLLBACK" = 1 ]; then
  [ -x "$BIN.prev" ] || die "no $BIN.prev to go back to"
  cp -p "$BIN.prev" "$BIN.new" && mv -f "$BIN.new" "$BIN"
  say "restored $BIN from $BIN.prev"
  if [ "$HAVE_SYSTEMD" = 1 ]; then
    systemctl restart rustadsb
    restart_stale
  fi
  exit 0
fi

if [ "$UNINSTALL" = 1 ]; then
  if [ "$HAVE_SYSTEMD" = 1 ]; then
    systemctl disable --now rustadsb 2>/dev/null || true
  fi
  rm -f "$UNIT" "$BIN" "$BIN.prev" "$UDEV" "$BLACKLIST"
  [ "$HAVE_SYSTEMD" = 1 ] && systemctl daemon-reload
  if [ "$PURGE" = 1 ]; then
    rm -f "$CONF" "$LEGACY" "$MCP_TOKEN"
    rmdir "$(dirname "$CONF")" 2>/dev/null || true
    rm -rf /var/lib/rustadsb
    id "$USER_NAME" >/dev/null 2>&1 && userdel "$USER_NAME"
  fi
  if [ "$PURGE" = 1 ]; then say "rustadsb removed, with its configuration and user"; else say "rustadsb removed (kept $CONF)"; fi
  exit 0
fi

# --- which build -------------------------------------------------------------------------------
# PLAT is the archive name (rustadsb-vX.Y.Z-linux-armv7.tar.gz); TRIPLE the Rust target name that
# older releases used instead (rustadsb-v0.1.0-armv7-unknown-linux-gnueabihf.tar.gz), tried second
case "$(uname -m)" in
  x86_64|amd64) PLAT=linux-x86_64; TRIPLE=x86_64-unknown-linux-gnu ;;
  aarch64|arm64) PLAT=linux-arm64; TRIPLE=aarch64-unknown-linux-gnu ;;
  armv7l|armv8l) PLAT=linux-armv7; TRIPLE=armv7-unknown-linux-gnueabihf ;;
  armv6l) PLAT=linux-armv6; TRIPLE=arm-unknown-linux-gnueabihf ;;
  *) die "no prebuilt binary for $(uname -m); build from source (see the README)" ;;
esac
# a 32-bit userland on a 64-bit Pi kernel reports aarch64 but needs the armv7 build
if [ "$PLAT" = linux-arm64 ] && [ "$(getconf LONG_BIT)" = 32 ]; then
  PLAT=linux-armv7; TRIPLE=armv7-unknown-linux-gnueabihf
fi
GLIBC=$(ldd --version 2>/dev/null | head -1 | grep -oE '[0-9]+\.[0-9]+$' || true)
if [ -n "$GLIBC" ] && [ "$(printf '%s\n2.31\n' "$GLIBC" | sort -V | head -1)" != 2.31 ]; then
  die "glibc $GLIBC is older than 2.31 (Debian 11 / Raspberry Pi OS Bullseye); upgrade or build from source"
fi

TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

fetch() { # url dest
  if command -v curl >/dev/null; then
    if [ -n "$TOKEN" ]; then curl -fsSL -H "Authorization: token $TOKEN" -o "$2" "$1"; else curl -fsSL -o "$2" "$1"; fi
  elif command -v wget >/dev/null; then
    if [ -n "$TOKEN" ]; then wget -q --header="Authorization: token $TOKEN" -O "$2" "$1"; else wget -qO "$2" "$1"; fi
  else die "need curl or wget"; fi
}

if [ -n "$ARCHIVE" ]; then
  [ -f "$ARCHIVE" ] || die "$ARCHIVE not found"
  cp "$ARCHIVE" "$TMP/"
  NAME=$(basename "$ARCHIVE")
  SUMS="$(dirname "$ARCHIVE")/SHA256SUMS"
  if [ -f "$SUMS" ]; then cp "$SUMS" "$TMP/SHA256SUMS"; else echo "warning: no SHA256SUMS next to the archive; not checked" >&2; fi
else
  [ -n "$REPO_URL" ] || die "no repository to download from: set RUSTADSB_REPO_URL, or use --archive"
  if [ "$VERSION" = latest ]; then
    case "$REPO_URL" in
      https://github.com/*) API="https://api.github.com/repos/${REPO_URL#https://github.com/}/releases/latest" ;;
      *) API="$(printf '%s' "$REPO_URL" | sed -E 's#^(https?://[^/]+)/(.*)$#\1/api/v1/repos/\2#')/releases/latest" ;;
    esac
    fetch "$API" "$TMP/latest.json" || die "could not ask $REPO_URL for the latest release (set --version)"
    VERSION=$(grep -oE '"tag_name": *"[^"]+"' "$TMP/latest.json" | head -1 | sed -E 's/.*"([^"]+)"$/\1/')
    [ -n "$VERSION" ] || die "no release found at $REPO_URL"
  fi
  BASE="$REPO_URL/releases/download/$VERSION"
  NAME="rustadsb-$VERSION-$PLAT.tar.gz"
  say "downloading $NAME"
  if ! fetch "$BASE/$NAME" "$TMP/$NAME" 2>/dev/null; then
    NAME="rustadsb-$VERSION-$TRIPLE.tar.gz" # older releases
    say "not found; trying the older name $NAME"
    fetch "$BASE/$NAME" "$TMP/$NAME" || die "download failed: $BASE/rustadsb-$VERSION-$PLAT.tar.gz"
  fi
  fetch "$BASE/SHA256SUMS" "$TMP/SHA256SUMS" || die "download failed: $BASE/SHA256SUMS"
fi

if [ -f "$TMP/SHA256SUMS" ]; then
  (cd "$TMP" && grep " $NAME\$" SHA256SUMS | sha256sum -c --quiet -) || die "checksum mismatch for $NAME"
  say "checksum OK"
fi
tar -xzf "$TMP/$NAME" -C "$TMP"
NEW=$(find "$TMP" -type f -name rustadsb | head -1)
[ -n "$NEW" ] || die "no rustadsb binary in $NAME"
"$NEW" --help >/dev/null 2>&1 || die "the downloaded binary does not run on this system ($PLAT)"

# --- another decoder holding the dongle? ----------------------------------------------------------
OTHERS=""
if [ "$HAVE_SYSTEMD" = 1 ]; then
  for s in dump1090-fa dump1090-mutability dump1090; do
    systemctl is-active --quiet "$s" 2>/dev/null && OTHERS="$OTHERS $s"
  done
fi
if [ -n "$OTHERS" ] && [ "$START" = 1 ]; then
  [ "$REPLACE" = 1 ] || die "already running:$OTHERS. They hold the dongle: rerun with --replace to stop and disable them (or --no-start)"
fi

# --- install ---------------------------------------------------------------------------------------
if [ -x "$BIN" ] && cmp -s "$BIN" "$NEW"; then
  say "$BIN is already this build ($NAME)"
else
  if [ -x "$BIN" ]; then
    cp -p "$BIN" "$BIN.prev"
    say "kept the previous binary as $BIN.prev"
  fi
  install -m 0755 "$NEW" "$BIN.new" && mv -f "$BIN.new" "$BIN"
  say "installed $BIN from $NAME"
fi

getent group plugdev >/dev/null || groupadd --system plugdev
if ! id "$USER_NAME" >/dev/null 2>&1; then
  useradd --system --no-create-home --shell /usr/sbin/nologin --groups plugdev "$USER_NAME"
  say "created user $USER_NAME"
else
  usermod -a -G plugdev "$USER_NAME"
fi

mkdir -p "$(dirname "$UDEV")" "$(dirname "$BLACKLIST")" "$(dirname "$UNIT")"
cat >"$UDEV" <<'EOF'
# rustadsb: RTL2832U dongles readable by plugdev (the rustadsb user is in it)
SUBSYSTEMS=="usb", ATTRS{idVendor}=="0bda", ATTRS{idProduct}=="2832", MODE="0664", GROUP="plugdev"
SUBSYSTEMS=="usb", ATTRS{idVendor}=="0bda", ATTRS{idProduct}=="2838", MODE="0664", GROUP="plugdev"
EOF
cat >"$BLACKLIST" <<'EOF'
# rustadsb: the DVB-T TV driver would claim the RTL-SDR dongle before the decoder can
blacklist dvb_usb_rtl28xxu
blacklist rtl2832
blacklist rtl2830
EOF
if command -v udevadm >/dev/null; then udevadm control --reload-rules && udevadm trigger --subsystem-match=usb || true; fi
if lsmod 2>/dev/null | grep -q '^dvb_usb_rtl28xxu'; then
  modprobe -r dvb_usb_rtl28xxu 2>/dev/null || echo "warning: the DVB-T driver is loaded and could not be removed: reboot once" >&2
fi

# --- configuration ---------------------------------------------------------------------------------
# rustadsb --config /etc/rustadsb/rustadsb.toml: one key per long option. Installs from before 0.4 have
# their options as shell variables in /etc/default/rustadsb; that keeps working until --migrate-config.
MODE=toml
if [ -f "$CONF" ]; then
  say "kept your $CONF"
  [ ! -f "$LEGACY" ] || echo "note: $LEGACY is not read any more (the options are in $CONF); remove it when you like" >&2
  if [ -n "$DASHBOARD_PORT" ] && ! grep -v '^[[:space:]]*#' "$CONF" | grep -q 'dashboard-port'; then
    echo "note: $CONF is kept as it is; to turn the dashboard on, add dashboard-port = $DASHBOARD_PORT there and restart" >&2
  fi
elif [ -f "$LEGACY" ] && [ "$MIGRATE" = 1 ]; then
  # the old unit gave these on its own command line
  OLD_JSON=$(sed -n 's|^ExecStart=.* --write-json \([^ ]*\).*|\1|p' "$UNIT" 2>/dev/null | head -1)
  JSON_DIR=${OLD_JSON:-$JSON_DIR}
  # shellcheck source=/dev/null
  OPTS=$(set +u; . "$LEGACY"; echo "${RECEIVER_OPTIONS:-} ${DECODER_OPTIONS:-} ${NET_OPTIONS:-} ${JSON_OPTIONS:-}")
  mkdir -p "$(dirname "$CONF")"
  # rustadsb itself turns the options into TOML, and must read the result back
  # shellcheck disable=SC2086
  "$NEW" $OPTS --write-json "$JSON_DIR" --write-state /var/lib/rustadsb --quiet --print-config >"$CONF.new" ||
    die "could not convert $LEGACY (nothing was changed)"
  "$NEW" --config "$CONF.new" --print-config >/dev/null || die "the converted $CONF.new does not read back (nothing was changed)"
  {
    echo "# Converted from $LEGACY by install.sh --migrate-config on $(date -u +%F)."
    echo "# Edit, then: sudo systemctl restart rustadsb. The installer never overwrites this file."
    cat "$CONF.new"
  } >"$CONF"
  rm -f "$CONF.new"
  OLD_KEPT="$LEGACY.migrated-$(date +%Y%m%d-%H%M%S)"
  mv "$LEGACY" "$OLD_KEPT"
  say "converted $LEGACY to $CONF (the old file is now $OLD_KEPT)"
elif [ -f "$LEGACY" ]; then
  MODE=legacy
  say "kept your $LEGACY (install.sh --migrate-config moves it to $CONF)"
  if [ -n "$DASHBOARD_PORT" ] && ! grep -q -- '--dashboard-port' "$LEGACY"; then
    echo "note: $LEGACY is kept as it is; to turn the dashboard on, add --dashboard-port $DASHBOARD_PORT to NET_OPTIONS there and restart" >&2
  fi
else
  if [ -n "$LAT" ] && [ -n "$LON" ]; then POS="lat = $LAT
lon = $LON"; else
    POS="# lat = 51.5                         # set both, then restart
# lon = -0.1"
    echo "note: no --lat/--lon given; set lat and lon in $CONF for the range outline and single-message positions" >&2
  fi
  MCP_LINES="# mcp-port = 8091                    # MCP server for AI assistants (needs mcp-token-file)
# mcp-token-file = \"$MCP_TOKEN\"   # install.sh --mcp-port makes one"
  [ -z "$MCP_PORT" ] || MCP_LINES="mcp-port = $MCP_PORT                     # MCP server for AI assistants: http://<this host>:$MCP_PORT/mcp
mcp-token-file = \"$MCP_TOKEN\"   # clients send: Authorization: Bearer <the token in it>"
  DASH_LINE="# dashboard-port = 8090              # the status web page: http://<this host>:8090/"
  [ -z "$DASHBOARD_PORT" ] || DASH_LINE="dashboard-port = $DASHBOARD_PORT                # the status web page: http://<this host>:$DASHBOARD_PORT/"
  mkdir -p "$(dirname "$CONF")"
  EXAMPLE=$(find "$TMP" -type f -name rustadsb.example.toml | head -1)
  if [ -n "$EXAMPLE" ]; then
    # the release's own commented example, with this install's choices filled in
    sed -e "1,3s|example configuration|configuration|" \
        -e "s|^gain = [0-9.]*|gain = $GAIN|" \
        -e "s|^net-bind-address = \"[^\"]*\"|net-bind-address = \"$BIND\"|" \
        -e "s|^write-json = \"[^\"]*\"|write-json = \"$JSON_DIR\"|" \
        "$EXAMPLE" >"$CONF.new"
    if [ -n "$LAT" ] && [ -n "$LON" ]; then
      sed -i -e "s|^# lat = [-0-9.]*|lat = $LAT|" -e "s|^# lon = [-0-9.]*|lon = $LON|" "$CONF.new"
    fi
    [ -z "$DASHBOARD_PORT" ] || sed -i "s|^# dashboard-port = [0-9]*|dashboard-port = $DASHBOARD_PORT|" "$CONF.new"
    [ -z "$MCP_PORT" ] || sed -i -e "s|^# mcp-port = [0-9]*|mcp-port = $MCP_PORT|" -e "s|^# mcp-token-file = |mcp-token-file = |" "$CONF.new"
    if [ "$BIND" = 127.0.0.1 ]; then
      # data ports kept local: the page and MCP still answer on the network
      [ -z "$DASHBOARD_PORT" ] || sed -i "s|^# dashboard-bind-address = |dashboard-bind-address = |" "$CONF.new"
      [ -z "$MCP_PORT" ] || sed -i "s|^# mcp-bind-address = |mcp-bind-address = |" "$CONF.new"
    fi
    "$NEW" --config "$CONF.new" --print-config >/dev/null || die "the example config does not read (nothing was changed)"
    mv "$CONF.new" "$CONF"
    say "wrote $CONF"
  else
  cat >"$CONF" <<EOF
# rustadsb options, read by rustadsb.service (rustadsb --config $CONF).
# One key per long option, as rustadsb --help lists them; true turns an on/off option on.
# Edit, then: sudo systemctl restart rustadsb. The installer never overwrites this file.

# Receiver: the RTL-SDR dongle
device-type = "rtlsdr"
device = 0
gain = $GAIN                          # dB; 49.6 is the maximum
ppm = 0

# Decoder
$POS
max-range = 450                       # nautical miles
fix = true                            # repair single-bit errors
write-json-every = 1                  # seconds

# Network: Beast out on 30005 for feed clients and maps, MLAT results in on 30104
net = true
net-bind-address = "$BIND"            # "127.0.0.1": this host only
net-ri-port = 30001
net-ro-port = 30002
net-sbs-port = 30003
net-bi-port = "30004,30104"
net-bo-port = 30005
$DASH_LINE
# dashboard-bind-address = "0.0.0.0"  # the page on the LAN while net-bind-address keeps the data ports local
$MCP_LINES
# mcp-bind-address = "0.0.0.0"        # MCP on the LAN while net-bind-address keeps the data ports local
# mcp-tls-cert = "/etc/rustadsb/tls/cert.pem"   # HTTPS for MCP (PEM, leaf first; reloaded when renewed)
# mcp-tls-key = "/etc/rustadsb/tls/key.pem"

# Output
json-location-accuracy = 2
range-outline-hours = 24
write-json = "$JSON_DIR"              # must stay directly under /run (systemd creates it)
write-state = "/var/lib/rustadsb"     # the range outline survives restarts
quiet = true

# Optional extras:
# write-json-globe-index = true       # globe tiles and traces for tar1090
# net-api-port = 8080                 # the HTTP API (/re-api)
EOF
  say "wrote $CONF"
  fi
fi
if [ -n "$MCP_PORT" ]; then
  # a random token only root and the rustadsb service can read; kept if there already is one
  if [ ! -s "$MCP_TOKEN" ]; then
    mkdir -p "$(dirname "$MCP_TOKEN")"
    (umask 077; od -An -tx1 -N32 /dev/urandom | tr -d ' \n' >"$MCP_TOKEN")
    say "made an MCP token in $MCP_TOKEN (read it with: sudo cat $MCP_TOKEN)"
  fi
  chown "root:$(id -gn "$USER_NAME")" "$MCP_TOKEN" && chmod 0640 "$MCP_TOKEN"
  if [ "$MODE" = toml ] && ! grep -v '^[[:space:]]*#' "$CONF" | grep -q 'mcp-port'; then
    echo "note: $CONF is kept as it is; to turn MCP on, add there: mcp-port = $MCP_PORT and mcp-token-file = \"$MCP_TOKEN\", then restart" >&2
  fi
fi
if [ "$MODE" = toml ]; then
  # the unit's runtime directory follows the file's write-json
  CONF_JSON=$(sed -n 's|^[[:space:]]*write-json[[:space:]]*=[[:space:]]*"\([^"]*\)".*|\1|p' "$CONF" | head -1)
  JSON_DIR=${CONF_JSON:-$JSON_DIR}
fi

RUNDIR=$(basename "$JSON_DIR")
[ "$(dirname "$JSON_DIR")" = /run ] || die "--json-dir must be directly under /run (systemd creates it)"
if [ "$MODE" = toml ]; then
  OPTIONS_FILE=$CONF
  SERVICE_LINES="ExecStart=$BIN --config $CONF"
else
  OPTIONS_FILE=$LEGACY
  SERVICE_LINES="EnvironmentFile=$LEGACY
ExecStart=$BIN \$RECEIVER_OPTIONS \$DECODER_OPTIONS \$NET_OPTIONS \$JSON_OPTIONS --write-json $JSON_DIR --write-state /var/lib/rustadsb --quiet"
fi
cat >"$UNIT" <<EOF
# Installed by rustadsb's install.sh. Options: $OPTIONS_FILE
[Unit]
Description=rustadsb ADS-B decoder (RTL-SDR)
Documentation=${REPO_URL:-man:rustadsb}
Wants=network.target
After=network.target dump1090-fa.service dump1090-mutability.service
Conflicts=dump1090-fa.service dump1090-mutability.service

[Service]
User=$USER_NAME
RuntimeDirectory=$RUNDIR
RuntimeDirectoryMode=0755
StateDirectory=rustadsb
$SERVICE_LINES
Type=simple
Restart=always
RestartSec=15
Nice=-5

[Install]
WantedBy=multi-user.target
EOF
say "wrote $UNIT"

if [ "$HAVE_SYSTEMD" = 0 ]; then
  if [ "$MODE" = toml ]; then
    say "no systemd here: start it yourself, e.g. sudo -u $USER_NAME $BIN --config $CONF"
  else
    say "no systemd here: start it yourself as $USER_NAME with the options in $LEGACY"
  fi
  exit 0
fi
systemctl daemon-reload
if [ "$START" = 0 ]; then
  say "installed; start with: sudo systemctl enable --now rustadsb"
  exit 0
fi
for s in $OTHERS; do
  systemctl disable --now "$s"
  say "stopped and disabled $s (back with: sudo systemctl disable --now rustadsb && sudo systemctl enable --now $s)"
done
systemctl enable rustadsb >/dev/null 2>&1
systemctl restart rustadsb
for _ in $(seq 1 20); do
  [ -s "$JSON_DIR/aircraft.json" ] && break
  sleep 1
done
if systemctl is-active --quiet rustadsb && [ -s "$JSON_DIR/aircraft.json" ]; then
  restart_stale
  say "rustadsb is running: $JSON_DIR/aircraft.json, Beast on port 30005. Logs: journalctl -u rustadsb"
  DASH=$(grep -v '^[[:space:]]*#' "$OPTIONS_FILE" | grep -oE 'dashboard-port[ =]+[0-9]+' | grep -oE '[0-9]+$' | head -1 || true)
  [ -z "$DASH" ] || say "dashboard: http://$(hostname -I 2>/dev/null | awk '{print $1}'):$DASH/"
  MCP=$(grep -v '^[[:space:]]*#' "$OPTIONS_FILE" | grep -oE 'mcp-port[ =]+[0-9]+' | grep -oE '[0-9]+$' | head -1 || true)
  if [ -n "$MCP" ]; then
    SCHEME=http; grep -v '^[[:space:]]*#' "$OPTIONS_FILE" | grep -q 'mcp-tls-cert' && SCHEME=https
    say "MCP: $SCHEME://$(hostname -I 2>/dev/null | awk '{print $1}'):$MCP/mcp (token: sudo cat $MCP_TOKEN)"
  fi
else
  systemctl --no-pager status rustadsb | tail -15 >&2 || true
  die "rustadsb did not come up (is the dongle plugged in? reboot once if the DVB-T driver was loaded)"
fi

#!/usr/bin/env bash
# rustadsb installer: download a release, check it, and run it as a systemd service.
#
#   curl -fsSL https://raw.githubusercontent.com/p0bailey/rustadbtest/main/install.sh | sudo bash -s -- --lat 51.5 --lon -0.1
#   sudo ./install.sh --lat 51.5 --lon -0.1              # from a checkout
#   sudo ./install.sh --archive rustadsb-v0.2.0-linux-armv7.tar.gz --lat ... --lon ...
#   sudo ./install.sh --rollback                         # back to the previous binary
#   sudo ./install.sh --uninstall [--purge]
#
# What it does, on Debian, Raspberry Pi OS, Ubuntu or any Linux with systemd:
#   * picks the build for this CPU (x86_64, arm64, armv7, armv6), downloads it and its SHA256SUMS from
#     the release page and refuses to install if the checksum does not match;
#   * installs /usr/local/bin/rustadsb (the previous one is kept as rustadsb.prev);
#   * creates the system user rustadsb, in plugdev, and a udev rule that gives plugdev the RTL-SDR dongle;
#   * keeps the kernel's DVB-T driver off the dongle (it would claim it first);
#   * writes /etc/default/rustadsb (only on the first install: your edits are kept on upgrade) and
#     /etc/systemd/system/rustadsb.service, then starts it;
#   * after an upgrade, restarts any other service still running the old binary (feed clients that run
#     rustadsb --net-only, for example).
# Re-running the script upgrades the binary and keeps the configuration.
set -euo pipefail

# The repository releases are downloaded from (any GitHub or Gitea repository URL works).
DEFAULT_REPO_URL="https://github.com/p0bailey/rustadbtest"
REPO_URL="${RUSTADSB_REPO_URL:-$DEFAULT_REPO_URL}"
VERSION="${RUSTADSB_VERSION:-latest}"
PREFIX=/usr/local/bin
BIN="$PREFIX/rustadsb"
CONF=/etc/default/rustadsb
UNIT=/etc/systemd/system/rustadsb.service
UDEV=/etc/udev/rules.d/60-rustadsb-rtlsdr.rules
BLACKLIST=/etc/modprobe.d/rustadsb-blacklist-dvb.conf
USER_NAME=rustadsb

LAT="" LON="" GAIN="49.6" JSON_DIR="/run/rustadsb" BIND="0.0.0.0" ARCHIVE="" REPLACE=0 UNINSTALL=0 PURGE=0 START=1
ROLLBACK=0 TOKEN="${RUSTADSB_TOKEN:-}"

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
    rm -f "$CONF"
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

if [ ! -f "$CONF" ]; then
  POS=""
  if [ -n "$LAT" ] && [ -n "$LON" ]; then POS="--lat $LAT --lon $LON"; else
    echo "note: no --lat/--lon given; set them in $CONF for the range outline and single-message positions" >&2
  fi
  cat >"$CONF" <<EOF
# rustadsb options (read by rustadsb.service). All flags: rustadsb --help.
# Edit, then: sudo systemctl restart rustadsb. The installer never overwrites this file.
RECEIVER_OPTIONS="--device-type rtlsdr --device 0 --gain $GAIN --ppm 0"
DECODER_OPTIONS="$POS --max-range 450 --fix --write-json-every 1"
NET_OPTIONS="--net --net-bind-address $BIND --net-ri-port 30001 --net-ro-port 30002 --net-sbs-port 30003 --net-bi-port 30004,30104 --net-bo-port 30005"
JSON_OPTIONS="--json-location-accuracy 2 --range-outline-hours 24"
# Optional extras, add to JSON_OPTIONS or NET_OPTIONS:
#   --write-json-globe-index          globe tiles and traces for tar1090
#   --net-api-port 8080               the HTTP API (/re-api)
EOF
  say "wrote $CONF"
else
  say "kept your $CONF"
fi

RUNDIR=$(basename "$JSON_DIR")
[ "$(dirname "$JSON_DIR")" = /run ] || die "--json-dir must be directly under /run (systemd creates it)"
cat >"$UNIT" <<EOF
# Installed by rustadsb's install.sh. Options: $CONF
[Unit]
Description=rustadsb ADS-B decoder (RTL-SDR)
Documentation=${REPO_URL:-man:rustadsb}
Wants=network.target
After=network.target dump1090-fa.service dump1090-mutability.service
Conflicts=dump1090-fa.service dump1090-mutability.service

[Service]
EnvironmentFile=$CONF
User=$USER_NAME
RuntimeDirectory=$RUNDIR
RuntimeDirectoryMode=0755
StateDirectory=rustadsb
ExecStart=$BIN \$RECEIVER_OPTIONS \$DECODER_OPTIONS \$NET_OPTIONS \$JSON_OPTIONS --write-json $JSON_DIR --write-state /var/lib/rustadsb --quiet
Type=simple
Restart=always
RestartSec=15
Nice=-5

[Install]
WantedBy=multi-user.target
EOF
say "wrote $UNIT"

if [ "$HAVE_SYSTEMD" = 0 ]; then
  say "no systemd here: start it yourself, e.g. sudo -u $USER_NAME $BIN --device-type rtlsdr ... --write-json $JSON_DIR"
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
else
  systemctl --no-pager status rustadsb | tail -15 >&2 || true
  die "rustadsb did not come up (is the dongle plugged in? reboot once if the DVB-T driver was loaded)"
fi

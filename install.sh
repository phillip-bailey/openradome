#!/usr/bin/env bash
# OpenRadome installer: download a release of openradome, check it, and run it as a systemd service.
#
#   curl -fsSL https://raw.githubusercontent.com/phillip-bailey/openradome/main/install.sh | sudo bash -s -- --lat 51.5 --lon -0.1
#   sudo ./install.sh --lat 51.5 --lon -0.1              # from a checkout
#   sudo ./install.sh --archive openradome-v0.6.0-linux-armv7.tar.gz --lat ... --lon ...
#   sudo ./install.sh --rollback                         # back to the previous binary
#   sudo ./install.sh --uninstall [--purge]
#
# What it does, on Debian, Raspberry Pi OS, Ubuntu or any Linux with systemd:
#   * picks the build for this CPU (x86_64, arm64, armv7, armv6), downloads it and its SHA256SUMS from
#     the release page and refuses to install if the checksum does not match;
#   * installs /usr/local/bin/openradome (the previous one is kept as openradome.prev);
#   * creates the system user openradome, in plugdev, and a udev rule that gives plugdev the RTL-SDR dongle;
#   * keeps the kernel's DVB-T driver off the dongle (it would claim it first);
#   * writes /etc/openradome/openradome.toml (only on the first install: your edits are kept on upgrade) and
#     /etc/systemd/system/openradome.service, then starts it;
#   * moves an install of rustadsb (OpenRadome's name before 0.6) over: its service, user, options,
#     saved state, MCP token and TLS files, leaving the old paths as links so feed clients and other
#     readers keep working (see "moving from rustadsb" below);
#   * after an upgrade, restarts any other service still running the old binary (feed clients that run
#     openradome --net-only, for example).
# Re-running the script upgrades the binary and keeps the configuration.
set -euo pipefail

# The GitHub repository releases are downloaded from.
DEFAULT_REPO_URL="https://github.com/phillip-bailey/openradome"
REPO_URL="${OPENRADOME_REPO_URL:-${RUSTADSB_REPO_URL:-$DEFAULT_REPO_URL}}"
VERSION="${OPENRADOME_VERSION:-${RUSTADSB_VERSION:-latest}}"
PREFIX=/usr/local/bin
BIN="$PREFIX/openradome"
CONF=/etc/openradome/openradome.toml
LEGACY=/etc/default/rustadsb   # the options file of rustadsb before 0.4 (shell variables), still read until migrated
MCP_TOKEN=/etc/openradome/mcp-token
UNIT=/etc/systemd/system/openradome.service
UDEV=/etc/udev/rules.d/60-openradome-rtlsdr.rules
BLACKLIST=/etc/modprobe.d/openradome-blacklist-dvb.conf
COMPAT=/etc/tmpfiles.d/openradome-compat.conf   # /run/rustadsb -> /run/openradome at every boot (moved installs)
USER_NAME=openradome
# rustadsb, OpenRadome's name before 0.6: what an older install left behind
OLD_BIN="$PREFIX/rustadsb"
OLD_ETC=/etc/rustadsb
OLD_UNIT=/etc/systemd/system/rustadsb.service
OLD_USER=rustadsb

LAT="" LON="" GAIN="49.6" JSON_DIR="/run/openradome" BIND="0.0.0.0" ARCHIVE="" REPLACE=0 UNINSTALL=0 PURGE=0 START=1
ROLLBACK=0 DASHBOARD_PORT="" MIGRATE=0 MCP_PORT=""

usage() {
  sed -n '2,23p' "$0" 2>/dev/null | sed 's/^# \{0,1\}//'
  cat <<EOF
Options:
  --lat DEG --lon DEG   receiver position (needed for positions from single messages and the range outline)
  --gain DB             tuner gain (default 49.6, the maximum)
  --version vX.Y.Z      release to install (default: the latest)
  --archive FILE        install from a downloaded release archive instead (checked against SHA256SUMS
                        next to it if there is one)
  --json-dir DIR        where aircraft.json goes (default /run/openradome, directly under /run)
  --bind ADDR           address the network ports listen on (default 0.0.0.0; 127.0.0.1 = this host only)
  --dashboard-port PORT serve the status web page on PORT (off by default; see the README)
  --migrate-config      convert rustadsb's pre-0.4 /etc/default/rustadsb into /etc/openradome/openradome.toml
  --mcp-port PORT       serve the MCP server on PORT, with a new random token in /etc/openradome/mcp-token
  --replace             stop and disable dump1090 if it is running (it holds the dongle)
  --no-start            install everything but do not start the service
  --rollback            put the previous binary (openradome.prev) back and restart
  --uninstall [--purge] remove the service and binary (--purge also removes the configuration and user)
Environment: OPENRADOME_REPO_URL (a https://github.com/OWNER/REPO URL; default $REPO_URL), OPENRADOME_VERSION.
From a private GitHub repository, download the archive and SHA256SUMS with "gh release download" and use
--archive.
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

# Services other than openradome.service whose openradome (or rustadsb) process still runs a replaced (deleted) binary.
stale_units() {
  local pid
  for pid in $(pgrep -x openradome 2>/dev/null; pgrep -x rustadsb 2>/dev/null); do
    case "$(readlink "/proc/$pid/exe" 2>/dev/null)" in
      *"(deleted)") sed -n 's|.*/\([^/]*\.service\)$|\1|p' "/proc/$pid/cgroup" ;;
    esac
  done | sort -u | { grep -vx -e openradome.service -e rustadsb.service || true; }
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
    systemctl restart openradome
    restart_stale
  fi
  exit 0
fi

if [ "$UNINSTALL" = 1 ]; then
  if [ "$HAVE_SYSTEMD" = 1 ]; then
    systemctl disable --now openradome 2>/dev/null || true
  fi
  rm -f "$UNIT" "$BIN" "$BIN.prev" "$UDEV" "$BLACKLIST" "$COMPAT"
  # the links a move from rustadsb left
  [ ! -L "$OLD_BIN" ] || rm -f "$OLD_BIN"
  [ ! -L /run/rustadsb ] || rm -f /run/rustadsb
  [ "$HAVE_SYSTEMD" = 1 ] && systemctl daemon-reload
  if [ "$PURGE" = 1 ]; then
    rm -f "$CONF" "$LEGACY" "$MCP_TOKEN"
    rmdir "$(dirname "$CONF")" 2>/dev/null || true
    [ ! -L "$OLD_ETC" ] || rm -f "$OLD_ETC"
    rm -rf /var/lib/openradome
    id "$USER_NAME" >/dev/null 2>&1 && userdel "$USER_NAME"
  fi
  if [ "$PURGE" = 1 ]; then say "openradome removed, with its configuration and user"; else say "openradome removed (kept $CONF)"; fi
  exit 0
fi

# --- which build -------------------------------------------------------------------------------
# PLAT is the archive name (openradome-vX.Y.Z-linux-armv7.tar.gz); releases before 0.6 were called
# rustadsb-vX.Y.Z-linux-armv7.tar.gz, and before 0.3 used the Rust target (TRIPLE) instead
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

fetch() { # url dest; https only, redirects included (GitHub sends downloads on to its file servers)
  if command -v curl >/dev/null; then
    curl -fsSL --proto '=https' --proto-redir '=https' -o "$2" "$1"
  elif command -v wget >/dev/null; then
    wget -q --https-only -O "$2" "$1"
  else die "need curl or wget"; fi
}

if [ -n "$ARCHIVE" ]; then
  [ -f "$ARCHIVE" ] || die "$ARCHIVE not found"
  cp "$ARCHIVE" "$TMP/"
  NAME=$(basename "$ARCHIVE")
  SUMS="$(dirname "$ARCHIVE")/SHA256SUMS"
  if [ -f "$SUMS" ]; then cp "$SUMS" "$TMP/SHA256SUMS"; else echo "warning: no SHA256SUMS next to the archive; not checked" >&2; fi
else
  [ -n "$REPO_URL" ] || die "no repository to download from: set OPENRADOME_REPO_URL, or use --archive"
  case "$REPO_URL" in
    https://github.com/?*/?*) REPO_URL="${REPO_URL%/}" ;;
    *) die "OPENRADOME_REPO_URL must be a GitHub repository (https://github.com/OWNER/REPO), not $REPO_URL" ;;
  esac
  if [ "$VERSION" = latest ]; then
    API="https://api.github.com/repos/${REPO_URL#https://github.com/}/releases/latest"
    fetch "$API" "$TMP/latest.json" || die "could not ask $REPO_URL for the latest release (set --version)"
    VERSION=$(grep -oE '"tag_name": *"[^"]+"' "$TMP/latest.json" | head -1 | sed -E 's/.*"([^"]+)"$/\1/')
    [ -n "$VERSION" ] || die "no release found at $REPO_URL"
  fi
  BASE="$REPO_URL/releases/download/$VERSION"
  FOUND=""
  for NAME in "openradome-$VERSION-$PLAT.tar.gz" "rustadsb-$VERSION-$PLAT.tar.gz" "rustadsb-$VERSION-$TRIPLE.tar.gz"; do
    say "downloading $NAME"
    if fetch "$BASE/$NAME" "$TMP/$NAME" 2>/dev/null; then FOUND=1; break; fi
  done
  [ -n "$FOUND" ] || die "download failed: no openradome-$VERSION-$PLAT.tar.gz at $BASE"
  fetch "$BASE/SHA256SUMS" "$TMP/SHA256SUMS" || die "download failed: $BASE/SHA256SUMS"
fi

if [ -f "$TMP/SHA256SUMS" ]; then
  (cd "$TMP" && grep " $NAME\$" SHA256SUMS | sha256sum -c --quiet -) || die "checksum mismatch for $NAME"
  say "checksum OK"
fi
tar -xzf "$TMP/$NAME" -C "$TMP"
NEW=$(find "$TMP" -type f \( -name openradome -o -name rustadsb \) | head -1)
[ -n "$NEW" ] || die "no openradome binary in $NAME"
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

# --- moving from rustadsb (OpenRadome's name before 0.6) ---------------------------------------------
# Only once: when rustadsb is installed and openradome is not yet. The options file is converted and checked
# with the new binary before anything is touched; then the service is stopped and everything is moved,
# leaving the old paths as links: /usr/local/bin/rustadsb (feed clients start it), /etc/rustadsb (an
# outside job may copy TLS files there), /run/rustadsb (readers of aircraft.json). The user and group
# are renamed in place, so files they own keep their owner; the service answers to rustadsb.service too.
if [ ! -e "$UNIT" ] && { [ -e "$OLD_UNIT" ] || [ -d "$OLD_ETC" ] && [ ! -L "$OLD_ETC" ]; }; then
  say "moving rustadsb over to openradome"
  OLD_CONF="$OLD_ETC/rustadsb.toml"
  if [ -f "$OLD_CONF" ]; then
    sed -e 's|/run/rustadsb|/run/openradome|g' -e 's|/var/lib/rustadsb|/var/lib/openradome|g' -e 's|/etc/rustadsb/|/etc/openradome/|g' \
        -e 's|rustadsb\.toml|openradome.toml|g' -e 's|rustadsb|openradome|g' "$OLD_CONF" >"$TMP/openradome.toml"
    "$NEW" --config "$TMP/openradome.toml" --print-config >/dev/null ||
      die "your $OLD_CONF does not read once moved (nothing was changed)"
  fi
  if [ "$HAVE_SYSTEMD" = 1 ]; then
    systemctl disable --now rustadsb 2>/dev/null || true
  fi
  if id "$OLD_USER" >/dev/null 2>&1 && ! id "$USER_NAME" >/dev/null 2>&1; then
    usermod -l "$USER_NAME" "$OLD_USER"
    ! getent group "$OLD_USER" >/dev/null || groupmod -n "$USER_NAME" "$OLD_USER"
    say "renamed user and group $OLD_USER to $USER_NAME (same ids, so file owners are kept)"
  fi
  if [ -d "$OLD_ETC" ] && [ ! -L "$OLD_ETC" ] && [ ! -e /etc/openradome ]; then
    mv "$OLD_ETC" /etc/openradome
    ln -s /etc/openradome "$OLD_ETC"
    if [ -f /etc/openradome/rustadsb.toml ]; then
      cp -p /etc/openradome/rustadsb.toml /etc/openradome/rustadsb.toml.before-openradome
      install -m 0644 "$TMP/openradome.toml" "$CONF"
      rm -f /etc/openradome/rustadsb.toml
    fi
    say "moved $OLD_ETC to /etc/openradome ($OLD_ETC is now a link to it; your options are in $CONF)"
  fi
  if [ -d /var/lib/rustadsb ] && [ ! -e /var/lib/openradome ]; then
    mv /var/lib/rustadsb /var/lib/openradome
    say "moved /var/lib/rustadsb to /var/lib/openradome"
  fi
  if [ -x "$OLD_BIN" ] && [ ! -L "$OLD_BIN" ]; then
    cp -p "$OLD_BIN" "$BIN.prev"           # --rollback puts the last rustadsb back under the new name
  fi
  rm -f "$OLD_UNIT" "$OLD_BIN.prev" /etc/udev/rules.d/60-rustadsb-rtlsdr.rules /etc/modprobe.d/rustadsb-blacklist-dvb.conf
  MOVED=1
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
if [ "${MOVED:-0}" = 1 ] || [ -L "$OLD_BIN" ]; then
  # feed clients and other services may still start /usr/local/bin/rustadsb
  ln -sfn openradome "$OLD_BIN.new" && mv -Tf "$OLD_BIN.new" "$OLD_BIN"
  [ -d /run/openradome ] || mkdir -p /run/openradome
  mkdir -p "$(dirname "$COMPAT")"
  printf '# openradome (install.sh): rustadsb, its name before 0.6, wrote here\nL+ /run/rustadsb - - - - /run/openradome\n' >"$COMPAT"
  if [ -d /run/rustadsb ] && [ ! -L /run/rustadsb ]; then rm -rf /run/rustadsb; fi
  ln -sfn /run/openradome /run/rustadsb
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
# openradome: RTL2832U dongles readable by plugdev (the openradome user is in it)
SUBSYSTEMS=="usb", ATTRS{idVendor}=="0bda", ATTRS{idProduct}=="2832", MODE="0664", GROUP="plugdev"
SUBSYSTEMS=="usb", ATTRS{idVendor}=="0bda", ATTRS{idProduct}=="2838", MODE="0664", GROUP="plugdev"
EOF
cat >"$BLACKLIST" <<'EOF'
# openradome: the DVB-T TV driver would claim the RTL-SDR dongle before the decoder can
blacklist dvb_usb_rtl28xxu
blacklist rtl2832
blacklist rtl2830
EOF
if command -v udevadm >/dev/null; then udevadm control --reload-rules && udevadm trigger --subsystem-match=usb || true; fi
if lsmod 2>/dev/null | grep -q '^dvb_usb_rtl28xxu'; then
  modprobe -r dvb_usb_rtl28xxu 2>/dev/null || echo "warning: the DVB-T driver is loaded and could not be removed: reboot once" >&2
fi

# --- configuration ---------------------------------------------------------------------------------
# openradome --config /etc/openradome/openradome.toml: one key per long option. rustadsb installs from before 0.4
# have their options as shell variables in /etc/default/rustadsb; moving to openradome converts them.
[ "${MOVED:-0}" = 0 ] || MIGRATE=1
MODE=toml
if [ -f "$CONF" ]; then
  say "kept your $CONF"
  [ ! -f "$LEGACY" ] || echo "note: $LEGACY is not read any more (the options are in $CONF); remove it when you like" >&2
  if [ -n "$DASHBOARD_PORT" ] && ! grep -v '^[[:space:]]*#' "$CONF" | grep -q 'dashboard-port'; then
    echo "note: $CONF is kept as it is; to turn the dashboard on, add dashboard-port = $DASHBOARD_PORT there and restart" >&2
  fi
elif [ -f "$LEGACY" ] && [ "$MIGRATE" = 1 ]; then
  # the old unit gave these on its own command line
  OLD_JSON=$(sed -n 's|^ExecStart=.* --write-json \([^ ]*\).*|\1|p' "$UNIT" "$OLD_UNIT" 2>/dev/null | head -1)
  OLD_JSON=${OLD_JSON/#\/run\/rustadsb//run/openradome}
  JSON_DIR=${OLD_JSON:-$JSON_DIR}
  # shellcheck source=/dev/null
  OPTS=$(set +u; . "$LEGACY"; echo "${RECEIVER_OPTIONS:-} ${DECODER_OPTIONS:-} ${NET_OPTIONS:-} ${JSON_OPTIONS:-}")
  mkdir -p "$(dirname "$CONF")"
  # rustadsb itself turns the options into TOML, and must read the result back
  # shellcheck disable=SC2086
  "$NEW" $OPTS --write-json "$JSON_DIR" --write-state /var/lib/openradome --quiet --print-config >"$CONF.new" ||
    die "could not convert $LEGACY (nothing was changed)"
  "$NEW" --config "$CONF.new" --print-config >/dev/null || die "the converted $CONF.new does not read back (nothing was changed)"
  {
    echo "# Converted from $LEGACY by install.sh --migrate-config on $(date -u +%F)."
    echo "# Edit, then: sudo systemctl restart openradome. The installer never overwrites this file."
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
  EXAMPLE=$(find "$TMP" -type f \( -name openradome.example.toml -o -name rustadsb.example.toml \) | head -1)
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
# openradome options, read by openradome.service (openradome --config $CONF).
# One key per long option, as openradome --help lists them; true turns an on/off option on.
# Edit, then: sudo systemctl restart openradome. The installer never overwrites this file.

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
# mcp-tls-cert = "/etc/openradome/tls/cert.pem"   # HTTPS for MCP (PEM, leaf first; reloaded when renewed)
# mcp-tls-key = "/etc/openradome/tls/key.pem"

# Output
json-location-accuracy = 2
range-outline-hours = 24
write-json = "$JSON_DIR"              # must stay directly under /run (systemd creates it)
write-state = "/var/lib/openradome"       # the range outline survives restarts
quiet = true

# Optional extras:
# write-json-globe-index = true       # globe tiles and traces for tar1090
# net-api-port = 8080                 # the HTTP API (/re-api)
EOF
  say "wrote $CONF"
  fi
fi
if [ -n "$MCP_PORT" ]; then
  # a random token only root and the openradome service can read; kept if there already is one
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
ExecStart=$BIN \$RECEIVER_OPTIONS \$DECODER_OPTIONS \$NET_OPTIONS \$JSON_OPTIONS --write-json $JSON_DIR --write-state /var/lib/openradome --quiet"
fi
cat >"$UNIT" <<EOF
# Installed by OpenRadome's install.sh. Options: $OPTIONS_FILE
[Unit]
Description=OpenRadome ADS-B decoder (openradome, RTL-SDR)
Documentation=${REPO_URL:-man:openradome}
Wants=network.target
After=network.target dump1090-fa.service dump1090-mutability.service
Conflicts=dump1090-fa.service dump1090-mutability.service

[Service]
User=$USER_NAME
RuntimeDirectory=$RUNDIR
RuntimeDirectoryMode=0755
StateDirectory=openradome
$SERVICE_LINES
Type=simple
Restart=always
RestartSec=15
Nice=-5

[Install]
WantedBy=multi-user.target
# rustadsb was OpenRadome's name before 0.6: services ordered after it, and systemctl ... rustadsb, still work
Alias=rustadsb.service
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
  say "installed; start with: sudo systemctl enable --now openradome"
  exit 0
fi
for s in $OTHERS; do
  systemctl disable --now "$s"
  say "stopped and disabled $s (back with: sudo systemctl disable --now openradome && sudo systemctl enable --now $s)"
done
systemctl enable openradome >/dev/null 2>&1
systemctl restart openradome
for _ in $(seq 1 20); do
  [ -s "$JSON_DIR/aircraft.json" ] && break
  sleep 1
done
if systemctl is-active --quiet openradome && [ -s "$JSON_DIR/aircraft.json" ]; then
  restart_stale
  say "openradome is running: $JSON_DIR/aircraft.json, Beast on port 30005. Logs: journalctl -u openradome"
  DASH=$(grep -v '^[[:space:]]*#' "$OPTIONS_FILE" | grep -oE 'dashboard-port[ =]+[0-9]+' | grep -oE '[0-9]+$' | head -1 || true)
  [ -z "$DASH" ] || say "dashboard: http://$(hostname -I 2>/dev/null | awk '{print $1}'):$DASH/"
  MCP=$(grep -v '^[[:space:]]*#' "$OPTIONS_FILE" | grep -oE 'mcp-port[ =]+[0-9]+' | grep -oE '[0-9]+$' | head -1 || true)
  if [ -n "$MCP" ]; then
    SCHEME=http; grep -v '^[[:space:]]*#' "$OPTIONS_FILE" | grep -q 'mcp-tls-cert' && SCHEME=https
    say "MCP: $SCHEME://$(hostname -I 2>/dev/null | awk '{print $1}'):$MCP/mcp (token: sudo cat $MCP_TOKEN)"
  fi
else
  systemctl --no-pager status openradome | tail -15 >&2 || true
  die "openradome did not come up (is the dongle plugged in? reboot once if the DVB-T driver was loaded)"
fi

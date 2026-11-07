#!/usr/bin/env bash
# Builds the trindle-crash-reports .deb into packaging/out/ (or the folder given as the first argument).
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(dirname "$here")"
out="${1:-$here/out}"
version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"
arch="$(dpkg --print-architecture)"
stage="$(mktemp -d)"
chmod 0755 "$stage"
mkdir -p "$out"

cargo build --release --locked --manifest-path "$root/Cargo.toml"

install -d "$stage/usr/bin"
install -m 0755 "$root/target/release/trindle-crash-reports" "$stage/usr/bin/trindle-crash-reports"

install -d "$stage/usr/lib/systemd/user"
cat > "$stage/usr/lib/systemd/user/trindle-crash-reports.service" <<'UNIT'
[Unit]
Description=Send Trindle's crash reports that are switched on

[Service]
Type=oneshot
ExecStart=/usr/bin/trindle-crash-reports send
Nice=10
UNIT
cat > "$stage/usr/lib/systemd/user/trindle-crash-reports.timer" <<'UNIT'
[Unit]
Description=Send Trindle's crash reports once an hour

[Timer]
OnStartupSec=5min
OnUnitActiveSec=1h
RandomizedDelaySec=5min

[Install]
WantedBy=timers.target
UNIT
chmod 0644 "$stage/usr/lib/systemd/user/trindle-crash-reports."*

install -d "$stage/usr/share/doc/trindle-crash-reports"
install -m 0644 "$root/LICENSE" "$stage/usr/share/doc/trindle-crash-reports/copyright"

install -d "$stage/DEBIAN"
size="$(du -sk "$stage" | cut -f1)"
cat > "$stage/DEBIAN/control" <<CTRL
Package: trindle-crash-reports
Version: $version
Section: utils
Priority: optional
Architecture: $arch
Depends: systemd, curl
Enhances: trindle
Installed-Size: $size
Maintainer: Trindle <crash-reports@trindle.org>
Description: Send Trindle's crash reports, with nothing personal (Trindle add-on)
 When one of Trindle's own programs crashes, Trindle leaves a short note on
 this computer. This add-on sends those notes to Trindle's bug tracker once an
 hour, so faults get fixed. A report holds the program, its version and where
 in Trindle's code it went wrong: never a message, a file name or
 anything about other programs, and your name only if you saved one in
 Trindle Settings.
 .
 Crashes, errors and a rough description of the computer each have a switch
 in Trindle Settings, under Plugins, which also lists what was sent. Removing
 this package stops it all; its list of what was sent stays in your home
 folder until you delete it.
CTRL

cat > "$stage/DEBIAN/postinst" <<'POST'
#!/bin/sh
set -e
if [ "$1" = "configure" ]; then
    systemctl --global enable trindle-crash-reports.timer >/dev/null 2>&1 || true
    for who in $(loginctl list-users --no-legend 2>/dev/null | awk '{print $2}'); do
        systemctl --user -M "$who@" daemon-reload >/dev/null 2>&1 || true
        systemctl --user -M "$who@" start trindle-crash-reports.timer >/dev/null 2>&1 || true
    done
fi
exit 0
POST

cat > "$stage/DEBIAN/prerm" <<'PRE'
#!/bin/sh
set -e
if [ "$1" = "remove" ]; then
    for who in $(loginctl list-users --no-legend 2>/dev/null | awk '{print $2}'); do
        systemctl --user -M "$who@" stop trindle-crash-reports.timer trindle-crash-reports.service >/dev/null 2>&1 || true
    done
    systemctl --global disable trindle-crash-reports.timer >/dev/null 2>&1 || true
fi
exit 0
PRE

chmod 0755 "$stage/DEBIAN/postinst" "$stage/DEBIAN/prerm"

deb="$out/trindle-crash-reports_${version}_${arch}.deb"
dpkg-deb --build --root-owner-group "$stage" "$deb" >/dev/null
rm -rf "$stage"
echo "$deb"

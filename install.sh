#!/bin/sh
# Build ember-pad and install it for the current user.
#   ./install.sh                         build (nice'd) and install
#   EMBER_PAD_BINARY=<file> ./install.sh install a binary built elsewhere
#   EMBER_PAD_NO_SERVICE=1 ./install.sh  don't touch the systemd user unit
# Installs to ~/.local/share/ember-pad/ember-pad, links ~/.local/bin/ember-pad, copies the user unit
# (if missing) and restarts it when it is running.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
dest="$HOME/.local/share/ember-pad"

if [ -n "${EMBER_PAD_BINARY:-}" ]; then
  built=$EMBER_PAD_BINARY
else
  nice -n 15 cargo build --release --locked -j "${EMBER_PAD_JOBS:-6}" --manifest-path "$here/Cargo.toml"
  built="$here/target/release/ember-pad"
fi

mkdir -p "$dest" "$HOME/.local/bin"
install -m 755 "$built" "$dest/.ember-pad.new"
mv -f "$dest/.ember-pad.new" "$dest/ember-pad"   # atomic: a running copy keeps its old binary
ln -sfn "$dest/ember-pad" "$HOME/.local/bin/ember-pad"
echo "installed $dest/ember-pad"

[ -z "${EMBER_PAD_NO_SERVICE:-}" ] || exit 0
unit="$HOME/.config/systemd/user/ember-pad.service"
if [ ! -e "$unit" ]; then
  mkdir -p "$(dirname "$unit")"
  cp "$here/contrib/ember-pad.service" "$unit"
  systemctl --user daemon-reload
  echo "added $unit (enable it with: systemctl --user enable --now ember-pad)"
fi
if systemctl --user is-active --quiet ember-pad.service; then
  systemctl --user restart ember-pad.service && echo "restarted ember-pad.service"
fi

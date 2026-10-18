#!/bin/sh
# Installs fiio-tunes and fiio-tunes-gui with cargo, and the desktop entry
# that puts the window in the app launcher. Run from anywhere;
# `--uninstall` removes all three.
set -eu

repo=$(cd "$(dirname "$0")/.." && pwd)
root=${CARGO_INSTALL_ROOT:-${CARGO_HOME:-$HOME/.cargo}}
# The entry needs an absolute path; cargo reads a relative one from here.
case $root in /*) ;; *) root=$PWD/$root ;; esac
apps=${XDG_DATA_HOME:-$HOME/.local/share}/applications
entry=$apps/fiio-tunes.desktop

if [ "${1-}" = --uninstall ]; then
    cargo uninstall --root "$root" fiio-tunes fiio-tunes-gui || true
    rm -f "$entry"
    exit 0
fi

cargo install --locked --root "$root" --path "$repo/crates/fiio-tunes"
cargo install --locked --root "$root" --path "$repo/crates/fiio-tunes-gui"
# The launcher may not have cargo's bin folder on its PATH: the entry names
# the installed binary in full, quoted as the desktop entry spec says (\ " `
# and $ escaped inside the quotes, then every backslash doubled, % doubled).
exec=$(printf '%s' "$root/bin/fiio-tunes-gui" |
    sed -e 's/[\\"`$]/\\&/g' -e 's/\\/\\\\/g' -e 's/%/%%/g')
mkdir -p "$apps"
while IFS= read -r line; do
    case $line in
    Exec=*) printf 'Exec="%s"\n' "$exec" ;;
    *) printf '%s\n' "$line" ;;
    esac
done < "$repo/packaging/fiio-tunes.desktop" > "$entry.tmp"
mv "$entry.tmp" "$entry"
echo "installed $root/bin/fiio-tunes, $root/bin/fiio-tunes-gui and $entry"

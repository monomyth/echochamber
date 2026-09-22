#!/bin/sh
# Install echochamber onto your PATH (~/.cargo/bin).
set -eu

if ! command -v cargo >/dev/null 2>&1; then
  echo "Rust is not installed. Get it from https://rustup.rs and run this again." >&2
  exit 1
fi
if ! command -v ffmpeg >/dev/null 2>&1; then
  echo "ffmpeg is not installed. Install it, then run this again." >&2
  exit 1
fi

if [ -f "$0" ]; then
  root=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
  if [ -f "$root/Cargo.toml" ]; then
    cargo install --path "$root" --locked
  else
    cargo install --git https://github.com/monomyth/echochamber.git --locked
  fi
else
  # curl ... | sh  (there is no script file on disk)
  cargo install --git https://github.com/monomyth/echochamber.git --locked
fi

echo
echo "Installed. In the folder where you want the settings file, run:"
echo "  echochamber init && echochamber serve"

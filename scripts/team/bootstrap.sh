#!/bin/bash
# First run for a new teammate: gets Homebrew, GitHub sign-in and the private
# team kit, then opens its setup page. Contains no secrets; every account is
# your own.
#
#   /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/greg-prospyr/herdr-gpui/team/stable/scripts/team/bootstrap.sh)"
set -euo pipefail

kit_repo=greg-prospyr/herdr-team-kit
kit_dir="$HOME/Development/herdr-team-kit"

if ! xcode-select -p >/dev/null 2>&1; then
  echo "Install Apple's Command Line Tools in the window that opens, then run this command again."
  xcode-select --install >/dev/null 2>&1 || true
  exit 1
fi
if ! command -v brew >/dev/null 2>&1 && [ ! -x /opt/homebrew/bin/brew ]; then
  echo "Installing Homebrew (it asks for your Mac password)…"
  /bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"
fi
eval "$(/opt/homebrew/bin/brew shellenv 2>/dev/null || /usr/local/bin/brew shellenv)"
command -v gh >/dev/null 2>&1 || brew install gh
gh auth status >/dev/null 2>&1 || gh auth login --web --git-protocol https

if [ -d "$kit_dir/.git" ]; then
  git -C "$kit_dir" pull --ff-only --quiet || true
elif ! gh repo clone "$kit_repo" "$kit_dir" -- --quiet; then
  echo "Could not clone $kit_repo. Ask your admin to give your GitHub account access, then run this again."
  exit 1
fi
exec "$kit_dir/bin/onboard"

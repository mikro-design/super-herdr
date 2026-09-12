#!/usr/bin/env bash
# Add the Super-Herdr APT repository and install Super-Herdr.
#
# Published to the repository root, so the whole install is one line:
#
#   curl -fsSL https://mikro-design.github.io/apt/install.sh | sudo bash
#
# Nobody reads four commands off a README to try something. The commands are
# still in the README for anyone who would rather run them, and this does
# exactly those and nothing else, which is why it is short enough to read
# before piping it anywhere.
set -euo pipefail

repository="https://mikro-design.github.io/apt"
keyring="/usr/share/keyrings/super-herdr.gpg"
source_list="/etc/apt/sources.list.d/super-herdr.list"

if ! command -v apt-get > /dev/null 2>&1; then
  echo "This installs a Debian package and needs apt." >&2
  echo "On macOS or other Linux: brew install mikro-design/tap/super-herdr" >&2
  exit 1
fi

if [[ "$(id -u)" -ne 0 ]]; then
  echo "Run this as root: curl -fsSL ${repository}/install.sh | sudo bash" >&2
  exit 1
fi

architecture="$(dpkg --print-architecture)"
case "${architecture}" in
  amd64 | arm64) ;;
  *)
    echo "No Debian package is published for ${architecture}." >&2
    echo "Prebuilt archives and sources: https://github.com/mikro-design/super-herdr" >&2
    exit 1
    ;;
esac

for tool in curl gpg; do
  if ! command -v "${tool}" > /dev/null 2>&1; then
    echo "Installing ${tool}, which this needs to verify the repository."
    apt-get update -qq
    apt-get install -y --no-install-recommends "${tool}"
  fi
done

echo "Adding the Super-Herdr repository."
install -d -m 0755 "$(dirname "${keyring}")"

# Written through a temporary file so an interrupted download cannot leave a
# truncated keyring in place, which apt reports as a signature failure rather
# than as the half-written file it is.
staged="$(mktemp)"
trap 'rm -f "${staged}"' EXIT
curl -fsSL "${repository}/super-herdr.gpg" -o "${staged}"
if ! gpg --show-keys "${staged}" > /dev/null 2>&1; then
  echo "The downloaded key is not a valid OpenPGP key; refusing to install it." >&2
  exit 1
fi
install -m 0644 "${staged}" "${keyring}"

echo "deb [signed-by=${keyring}] ${repository} stable main" > "${source_list}"

echo "Updating package lists."
apt-get update -o Dir::Etc::sourcelist="${source_list}" \
  -o Dir::Etc::sourceparts=/dev/null -o APT::Get::List-Cleanup=0

echo "Installing super-herdr."
if ! apt-get install -y super-herdr; then
  # apt declines a transaction while another package is left half configured,
  # and it declines this one by naming whichever unrelated package is broken.
  # Somebody who ran a one-line installer and was told about libnvidia-compute
  # has no reason to connect the two, so the state is read here and explained.
  #
  # After the failure rather than before it, deliberately. Half-configured
  # packages do not always block — apt copes with plenty of them — and a check
  # that ran first would refuse installs that were going to succeed, which is
  # worse than the confusing error it set out to replace.
  broken="$(dpkg-query -W -f='${Package} ${Status}\n' 2> /dev/null \
    | awk '$NF == "unpacked" || $NF == "half-configured" || $NF == "half-installed" { print $1 }' \
    | sort -u | sed '/^$/d')"
  if [[ -n "${broken}" ]]; then
    count="$(echo "${broken}" | wc -l)"
    cat >&2 <<REASON

That failure is about this system, not about Super-Herdr. These packages are
not fully installed, and apt will not complete any transaction until they are:

$(echo "${broken}" | head -5 | sed 's/^/  /')$([[ "${count}" -gt 5 ]] && printf '\n  ... and %d more' "$(( count - 5 ))")

Sort them out, then run this again:

  sudo apt --fix-broken install

If that does not clear it, try: sudo dpkg --configure -a
REASON
  fi
  exit 1
fi

cat <<EOF

Installed $(super-herdr --version 2> /dev/null || echo super-herdr).

  super-herdr target add desktop --local --discover-sessions
  super-herdr

Upgrades arrive with apt upgrade from now on.
EOF

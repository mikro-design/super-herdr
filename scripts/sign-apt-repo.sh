#!/usr/bin/env bash
# Sign a rendered APT repository, and export the key that verifies it.
#
# Separate from rendering because it is the only step that needs the private
# key. Rendering is reproducible and runs anywhere; signing runs where the key
# is, which is deliberately not this repository's CI. See packaging/README.md.
#
# Both signatures are written. `InRelease` is what modern apt fetches, and
# `Release.gpg` is what older clients look for; publishing one and not the other
# strands whichever client asked for the missing file.
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <repository-directory> <signing-key-id>" >&2
  exit 2
fi

repo="$1"
key="$2"
suite="stable"
release="${repo}/dists/${suite}/Release"

if [[ ! -f "${release}" ]]; then
  echo "no ${release}; run scripts/render-apt-repo.sh first" >&2
  exit 1
fi

if ! gpg --list-secret-keys "${key}" > /dev/null 2>&1; then
  echo "no secret key for ${key} in this keyring" >&2
  exit 1
fi

# A passphrase-protected key needs somewhere to ask for the passphrase, and
# GnuPG asks on the terminal it was told about rather than the one it inherited.
# Without this a signature fails with "Inappropriate ioctl for device", which
# says nothing about passphrases to anyone who has not met it before.
if [[ -z "${GPG_TTY:-}" ]] && tty_name="$(tty 2> /dev/null)"; then
  export GPG_TTY="${tty_name}"
fi
if [[ -z "${GPG_TTY:-}" ]]; then
  echo "warning: no terminal for a passphrase prompt; run this from a shell" >&2
fi

gpg --batch --yes --default-key "${key}" \
  --clearsign --output "${repo}/dists/${suite}/InRelease" "${release}"
gpg --batch --yes --default-key "${key}" \
  --detach-sign --armor --output "${repo}/dists/${suite}/Release.gpg" "${release}"

# The public key ships beside the repository it verifies. Dearmoured, because a
# `signed-by` keyring is read as binary and an ASCII-armoured file placed there
# fails at verification rather than at download, where the cause is legible.
gpg --export "${key}" > "${repo}/super-herdr.gpg"

echo "signed ${suite} with ${key}"
echo "exported $(basename "${repo}")/super-herdr.gpg"

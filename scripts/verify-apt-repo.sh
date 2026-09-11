#!/usr/bin/env bash
# Verify a rendered APT repository against itself, before it is published.
#
# `apt` trusts an index, and the index is only as good as its agreement with the
# pool it describes. A stale index names a checksum no file has any more, and
# the failure surfaces on somebody else's machine as a hash mismatch with
# nothing to act on. Everything checked here is checked from the published
# bytes, the way a client would read them.
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <repository-directory>" >&2
  exit 2
fi

repo="$1"
suite="stable"
dist="${repo}/dists/${suite}"

for required in "${dist}/Release" "${dist}/InRelease" "${dist}/Release.gpg" "${repo}/super-herdr.gpg"; do
  if [[ ! -f "${required}" ]]; then
    echo "missing ${required}" >&2
    exit 1
  fi
done

# The signature verifies against the exported key, not against whatever the
# signing machine happens to trust. That is the check a client performs, and it
# catches signing with a key users were never given.
keyring="$(mktemp)"
trap 'rm -f "${keyring}"' EXIT
gpg --no-default-keyring --keyring "${keyring}" --import "${repo}/super-herdr.gpg" 2> /dev/null

for signature in InRelease Release.gpg; do
  if ! gpg --no-default-keyring --keyring "${keyring}" --verify \
      "${dist}/${signature}" $([[ "${signature}" == "Release.gpg" ]] && echo "${dist}/Release") \
      > /dev/null 2>&1; then
    echo "${signature} does not verify against the published key" >&2
    exit 1
  fi
done

# InRelease is a signed copy of Release rather than a second opinion. A client
# fetching one and a client fetching the other must be told the same thing.
if ! diff -q <(gpg --no-default-keyring --keyring "${keyring}" \
    --decrypt "${dist}/InRelease" 2> /dev/null) "${dist}/Release" > /dev/null; then
  echo "InRelease and Release disagree" >&2
  exit 1
fi

# Every index the Release file vouches for must still be the file on disk.
indexes=0
while read -r digest size path; do
  [[ -n "${path}" ]] || continue
  actual="${dist}/${path}"
  if [[ ! -f "${actual}" ]]; then
    echo "Release names ${path}, which is not in the repository" >&2
    exit 1
  fi
  if [[ "$(sha256sum "${actual}" | awk '{ print $1 }')" != "${digest}" ]]; then
    echo "${path} does not match the checksum in Release" >&2
    exit 1
  fi
  if [[ "$(stat -c %s "${actual}")" != "${size}" ]]; then
    echo "${path} does not match the size in Release" >&2
    exit 1
  fi
  indexes=$(( indexes + 1 ))
done < <(awk '/^SHA256:/ { inside = 1; next } /^[^ ]/ { inside = 0 } inside' "${dist}/Release")

if [[ "${indexes}" -eq 0 ]]; then
  echo "Release vouches for no indexes at all" >&2
  exit 1
fi

# And every package an index offers must be the file the pool holds. This is the
# checksum a client verifies after downloading, so a mismatch here is exactly
# the install failure it would cause.
offered=0
for packages in "${dist}"/*/binary-*/Packages; do
  [[ -f "${packages}" ]] || continue
  while read -r filename digest size; do
    package="${repo}/${filename}"
    if [[ ! -f "${package}" ]]; then
      echo "${packages} offers ${filename}, which is not in the pool" >&2
      exit 1
    fi
    if [[ "$(sha256sum "${package}" | awk '{ print $1 }')" != "${digest}" ]]; then
      echo "${filename} does not match the checksum in ${packages}" >&2
      exit 1
    fi
    if [[ "$(stat -c %s "${package}")" != "${size}" ]]; then
      echo "${filename} does not match the size in ${packages}" >&2
      exit 1
    fi
    offered=$(( offered + 1 ))
  done < <(awk '
    /^Filename: / { filename = $2 }
    /^SHA256: / { digest = $2 }
    /^Size: / { size = $2 }
    /^$/ { if (filename != "") print filename, digest, size; filename = "" }
    END { if (filename != "") print filename, digest, size }
  ' "${packages}")
done

if [[ "${offered}" -eq 0 ]]; then
  echo "no package is offered by any index" >&2
  exit 1
fi

echo "verified ${suite}: ${indexes} index(es), ${offered} package entr(ies), signed and consistent"

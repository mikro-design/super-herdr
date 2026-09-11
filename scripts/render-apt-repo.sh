#!/usr/bin/env bash
# Render an APT repository from the Debian packages already in its pool.
#
# `apt` does not verify packages; it verifies an index, and trusts the packages
# that index names by checksum. So the index is the whole security boundary,
# and it is generated here from the packages themselves rather than written
# down anywhere a copy could drift.
#
# The pool is the input: whatever `.deb` files are under pool/main/s/super-herdr
# are the versions this repository offers. Populating it is the caller's job,
# because which releases stay installable is a decision and not a detail.
#
# Signing is separate, in scripts/sign-apt-repo.sh. This script needs no key and
# is safe to run anywhere, which is what makes the index reproducible: the same
# pool renders the same bytes whether CI or a laptop rendered them.
set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 <repository-directory>" >&2
  exit 2
fi

repo="$1"
suite="stable"
component="main"
pool="pool/${component}/s/super-herdr"
architectures=(amd64 arm64)

if [[ ! -d "${repo}/${pool}" ]]; then
  echo "no pool at ${repo}/${pool}; nothing to publish" >&2
  exit 1
fi

packages="$(find "${repo}/${pool}" -name '*.deb' | wc -l)"
if [[ "${packages}" -eq 0 ]]; then
  echo "no Debian packages in ${repo}/${pool}" >&2
  exit 1
fi

# Rebuilt rather than updated. An index that accumulates edits is an index that
# can disagree with its pool, and the disagreement is invisible until an install
# fails against a checksum nobody rechecked.
rm -rf "${repo}/dists"

for architecture in "${architectures[@]}"; do
  binary="dists/${suite}/${component}/binary-${architecture}"
  mkdir -p "${repo}/${binary}"
  # Paths in the index are relative to the repository root, so the index is
  # rendered from there and stays correct under any URL it is served from.
  (
    cd "${repo}"
    apt-ftparchive --arch "${architecture}" packages "${pool}" > "${binary}/Packages"
    gzip -9 --keep --force "${binary}/Packages"
  )
  count="$(grep -c '^Package: ' "${repo}/${binary}/Packages" || true)"
  if [[ "${count}" -eq 0 ]]; then
    echo "no ${architecture} packages found in ${pool}" >&2
    exit 1
  fi
  echo "${architecture}: ${count} package(s)"
done

# `apt` refuses a Release file whose Suite, Codename or Architectures do not
# cover what the source line asked for, so these are stated rather than left to
# whatever apt-ftparchive infers from the directory it was pointed at.
#
# Written elsewhere and moved into place: a redirect creates its target before
# the command runs, and apt-ftparchive would then find an empty Release file in
# the directory it is indexing and list it as one of its own contents.
release="$(mktemp)"
trap 'rm -f "${release}"' EXIT

(
  cd "${repo}"
  apt-ftparchive \
    -o "APT::FTPArchive::Release::Origin=super-herdr" \
    -o "APT::FTPArchive::Release::Label=super-herdr" \
    -o "APT::FTPArchive::Release::Suite=${suite}" \
    -o "APT::FTPArchive::Release::Codename=${suite}" \
    -o "APT::FTPArchive::Release::Components=${component}" \
    -o "APT::FTPArchive::Release::Architectures=${architectures[*]}" \
    -o "APT::FTPArchive::Release::Description=Super-Herdr released Debian packages" \
    release "dists/${suite}" > "${release}"
)
mv "${release}" "${repo}/dists/${suite}/Release"
trap - EXIT

echo "rendered ${repo}/dists/${suite}/Release from ${packages} package(s)"

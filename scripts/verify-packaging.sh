#!/usr/bin/env bash
# Verify a release's package definitions against its own checksum manifest.
#
# The formula, the PKGBUILD, and .SRCINFO each restate checksums a release
# already published, and a restated value drifts silently. They are checked
# here rather than trusted, before anything is published or submitted to a
# package manager.
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <version> <release-files-directory>" >&2
  exit 2
fi

version="$1"
dir="$2"
sums="${dir}/SHA256SUMS"
formula="${dir}/super-herdr.rb"
pkgbuild="${dir}/PKGBUILD"
srcinfo="${dir}/super-herdr-bin.SRCINFO"

for file in "${sums}" "${formula}" "${pkgbuild}" "${srcinfo}"; do
  if [[ ! -f "${file}" ]]; then
    echo "missing ${file}" >&2
    exit 1
  fi
done

failures=0
fail() {
  echo "FAIL: $*" >&2
  failures=$((failures + 1))
}

listed() {
  awk -v name="$1" '$2 == name || $2 == "*"name { found = 1 } END { exit !found }' "${sums}"
}

contains() {
  grep -qF -- "$2" "$1" || fail "$3"
}

# A file that ships without a manifest line ships unsigned.
while IFS= read -r published; do
  name="$(basename "${published}")"
  listed "${name}" || fail "${name} is not listed in SHA256SUMS"
done < <(find "${dir}" -maxdepth 1 -type f \( -name '*.tar.gz' -o -name '*.deb' \))

for target in \
  aarch64-apple-darwin \
  x86_64-apple-darwin \
  aarch64-unknown-linux-gnu \
  x86_64-unknown-linux-gnu; do
  archive="super-herdr-v${version}-${target}.tar.gz"
  value="$(awk -v archive="${archive}" \
    '$2 == archive || $2 == "*"archive { print $1; exit }' "${sums}")"
  if [[ -z "${value}" ]]; then
    fail "no checksum for ${archive} in SHA256SUMS"
    continue
  fi

  contains "${formula}" "${archive}" "the formula omits ${archive}"
  contains "${formula}" "${value}" "the formula omits the ${target} checksum"

  # Only the Linux archives are packaged for the AUR.
  if [[ "${target}" == *-unknown-linux-gnu ]]; then
    arch="${target%%-*}"
    contains "${pkgbuild}" "sha256sums_${arch}=('${value}')" \
      "the PKGBUILD omits the ${target} checksum"
    contains "${srcinfo}" "sha256sums_${arch} = ${value}" \
      ".SRCINFO omits the ${target} checksum"
    contains "${srcinfo}" "source_${arch} = " ".SRCINFO omits the ${arch} source"
  fi
done

# The AUR validates a push against .SRCINFO, so it must not lag the PKGBUILD.
contains "${formula}" "version \"${version}\"" "the formula does not name ${version}"
contains "${pkgbuild}" "pkgver=${version}" "the PKGBUILD does not name ${version}"
contains "${srcinfo}" "pkgver = ${version}" ".SRCINFO does not name ${version}"

if (( failures > 0 )); then
  echo "${failures} package definition check(s) failed" >&2
  exit 1
fi

echo "package definitions for v${version} match SHA256SUMS"

#!/usr/bin/env bash
# Publish one release into a checked-out APT repository.
#
# The Debian analogue of the manual Homebrew step in packaging/README.md, and
# manual for the same reason: it signs, and the signing key deliberately does
# not live in this repository's CI. What it automates is everything either side
# of the signature, because those are the steps that are easy to get subtly
# wrong by hand.
#
# Packages are taken from the GitHub release and checked against that release's
# own SHA256SUMS before they are indexed. A repository that serves a package the
# release did not publish is the failure this exists to prevent, so it is
# checked here rather than assumed from a successful download.
set -euo pipefail

if [[ $# -lt 3 ]]; then
  echo "usage: $0 <version> <apt-repo-checkout> <signing-key-id> [keep]" >&2
  echo "  keep: how many releases to leave installable (default 10)" >&2
  exit 2
fi

version="$1"
repo="$2"
key="$3"
keep="${4:-10}"
source_repo="${SUPER_HERDR_REPO:-mikro-design/super-herdr}"
pool="${repo}/pool/main/s/super-herdr"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [[ ! -d "${repo}/.git" ]]; then
  echo "${repo} is not a git checkout of the apt repository" >&2
  exit 1
fi

staging="$(mktemp -d)"
trap 'rm -rf "${staging}"' EXIT

echo "fetching v${version} from ${source_repo}"
gh release download "v${version}" --repo "${source_repo}" --dir "${staging}" \
  --pattern '*.deb' --pattern 'SHA256SUMS'

# The release's own manifest is the authority on what it published. Checking
# against it here means a truncated download, a wrong tag, or an asset replaced
# after the fact fails now rather than on somebody's machine.
echo "verifying against the release manifest"
(cd "${staging}" && sha256sum -c --ignore-missing SHA256SUMS)

packages="$(find "${staging}" -name '*.deb' | wc -l)"
if [[ "${packages}" -ne 2 ]]; then
  echo "expected 2 Debian packages in v${version}, found ${packages}" >&2
  exit 1
fi

mkdir -p "${pool}"
cp "${staging}"/*.deb "${pool}/"

# Old versions stay installable so `apt install super-herdr=0.7.22-1` works and
# a bad release can be stepped back from, but not forever: the pool is served
# from a static host with a size budget, and nobody pins a version from a year
# ago. Sorted by Debian version rather than by name, because 0.7.9 sorts after
# 0.7.10 as a string and pruning the wrong one is silent.
mapfile -t versions < <(
  find "${pool}" -name '*.deb' -print0 \
    | xargs -0 -r -n1 dpkg-deb --field 2>/dev/null \
    | awk '/^Version: /{ print $2 }' \
    | sort -u -V
)
if [[ "${#versions[@]}" -gt "${keep}" ]]; then
  drop=$(( ${#versions[@]} - keep ))
  for stale in "${versions[@]:0:${drop}}"; do
    echo "pruning ${stale}"
    find "${pool}" -name "super-herdr_${stale}_*.deb" -delete
  done
fi

"${here}/render-apt-repo.sh" "${repo}"
"${here}/sign-apt-repo.sh" "${repo}" "${key}"
"${here}/verify-apt-repo.sh" "${repo}"

cat <<EOF

Rendered and signed. Nothing is published until this is pushed:

  git -C "${repo}" add -A
  git -C "${repo}" commit -m "super-herdr ${version}"
  git -C "${repo}" push origin main
EOF

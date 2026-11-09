#!/bin/sh
# trunk post_build hook: stamps public/sw.js with this build's id and the
# list of shell files to precache. The id is a hash of every staged file
# (names and contents), so any change to the app yields a new cache and the
# old one is deleted on activation.
set -eu

dir="${TRUNK_STAGING_DIR:?must be run by trunk as a post_build hook}"
cd "$dir"

files=$(find . -type f ! -name sw.js | sed 's|^\./||' | LC_ALL=C sort)
[ -n "$files" ] || { echo "stamp-sw: no staged files" >&2; exit 1; }

# shellcheck disable=SC2086 # word splitting is intended: trunk names have no spaces
build=$(sha256sum $files | sha256sum | cut -c1-16)
shell=$(printf '%s\n' $files | sed 's|.*|"/&"|' | paste -sd, -)

sed -i \
  -e "s|__PASSPORTER_BUILD__|$build|" \
  -e "s|__PASSPORTER_SHELL__|[$shell]|" \
  sw.js
echo "stamp-sw: build $build, $(printf '%s\n' $files | wc -l) shell files"

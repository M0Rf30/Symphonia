#!/usr/bin/env bash
# Print the body of the CHANGELOG.md section for VERSION (without the heading).
#
# Usage: changelog-section.sh VERSION [CHANGELOG]
#
# Fails when the section is missing or empty, so a release cannot go out without notes.
set -euo pipefail

version=${1:?usage: changelog-section.sh VERSION [CHANGELOG]}
changelog=${2:-CHANGELOG.md}

notes=$(awk -v version="$version" '
    /^## / {
        if (in_section) { exit }
        in_section = (index($0, "## [" version "]") == 1)
        next
    }
    in_section { print }
' "$changelog")

if [[ -z "${notes//[[:space:]]/}" ]]; then
    echo "error: no non-empty '## [${version}]' section in ${changelog}" >&2
    exit 1
fi

printf '%s\n' "$notes"

#!/usr/bin/env bash
# Build every non-merge commit in BASE..HEAD, oldest first, so that each intermediate commit of a
# pull request compiles on its own (otherwise `git bisect` lands on broken commits).
#
# Usage: check-commits.sh BASE HEAD
#
# Every commit is checked even after a failure so that all broken commits are reported at once.
set -euo pipefail

base=${1:?usage: check-commits.sh BASE HEAD}
head=${2:?usage: check-commits.sh BASE HEAD}

mapfile -t commits < <(git rev-list --reverse --no-merges "${base}..${head}")
echo "Checking ${#commits[@]} commit(s) in ${base}..${head}"

failed=()
for commit in "${commits[@]}"; do
    subject=$(git log -1 --format='%h %s' "$commit")
    git checkout --quiet --detach "$commit"
    echo "::group::${subject}"
    if ! cargo check --workspace --all-targets --all-features; then
        failed+=("$subject")
        echo "::error title=Commit does not build::${subject}"
    fi
    echo "::endgroup::"
done

git checkout --quiet --detach "$head"

if ((${#failed[@]} > 0)); then
    echo "These commits do not build:"
    printf '  %s\n' "${failed[@]}"
    exit 1
fi

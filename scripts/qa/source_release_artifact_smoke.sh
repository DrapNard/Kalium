#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 2 || $# -gt 2 ]]; then
  echo "usage: $0 <reviewed-tag> <empty-output-directory>" >&2
  exit 2
fi

tag=$1
output_dir=$2
repo_root=$(git rev-parse --show-toplevel)
commit=$(git -C "$repo_root" rev-parse --verify "refs/tags/$tag^{commit}")
tag_slug=${tag//\//_}

for command in git zipinfo unzip sha256sum tar grep; do
  command -v "$command" >/dev/null 2>&1 || {
    echo "required command not found: $command" >&2
    exit 1
  }
done

mkdir -p "$output_dir"
output_dir=$(cd "$output_dir" && pwd)
source_zip="$output_dir/Kalium-$tag_slug-source.zip"
git_bundle="$output_dir/Kalium-$tag_slug-history.bundle"
checksums="$output_dir/SHA256SUMS"

for artifact in "$source_zip" "$git_bundle" "$checksums"; do
  if [[ -e "$artifact" ]]; then
    echo "refusing to overwrite existing artifact: $artifact" >&2
    exit 1
  fi
done

# Artifacts are built from the reviewed tag, never from a possibly dirty worktree.
git -C "$repo_root" archive --format=zip --prefix="Kalium-$tag_slug/" "$tag" >"$source_zip"
git -C "$repo_root" bundle create "$git_bundle" "$tag" --tags
(
  cd "$output_dir"
  sha256sum "$(basename "$source_zip")" "$(basename "$git_bundle")" >"$(basename "$checksums")"
  sha256sum --check "$(basename "$checksums")"
)

git -C "$repo_root" bundle verify "$git_bundle" >/dev/null
expected_entries=$(
  {
    printf 'Kalium-%s/\n' "$tag_slug"
    git -C "$repo_root" archive --format=tar "$tag" | tar -tf - | sed "s#^#Kalium-$tag_slug/#"
  } | sort
)
actual_entries=$(zipinfo -1 "$source_zip" | sort)
[[ "$expected_entries" == "$actual_entries" ]]
zipinfo -1 "$source_zip" | grep -Fx "Kalium-$tag_slug/Cargo.toml" >/dev/null
zipinfo -1 "$source_zip" | grep -Fx "Kalium-$tag_slug/src/main.rs" >/dev/null
unzip -tq "$source_zip" >/dev/null

verify_dir=$(mktemp -d)
trap 'rm -rf "$verify_dir"' EXIT
mkdir "$verify_dir/source" "$verify_dir/clone"
unzip -q "$source_zip" -d "$verify_dir/source"
git -c init.defaultBranch=main clone --quiet --no-checkout "$git_bundle" "$verify_dir/clone"
git -C "$verify_dir/clone" checkout --quiet --detach "$tag"
git -C "$verify_dir/clone" fsck --full --no-reflogs

verified_commit=$(git -C "$verify_dir/clone" rev-parse HEAD)
[[ "$verified_commit" == "$commit" ]]
[[ -f "$verify_dir/source/Kalium-$tag_slug/Cargo.toml" ]]
[[ -f "$verify_dir/source/Kalium-$tag_slug/src/main.rs" ]]
[[ ! -e "$verify_dir/source/Kalium-$tag_slug/.git" ]]
[[ "$(git -C "$verify_dir/clone" tag --list | sort)" == "$(git -C "$repo_root" tag --list | sort)" ]]

echo "Verified source release for $tag ($commit):"
echo "  $source_zip"
echo "  $git_bundle"
echo "  $checksums"

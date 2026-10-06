# Development env inside the airlock sandbox (IS_SANDBOX=1). Sourced by
# mise (`_.source` in mise.toml); does nothing on the host.
#
# Build outputs go to the /cache disk instead of the checkout:
# - AIRLOCK_DEV_CACHE_DIR: artifacts shared by the main checkout and all
#   worktrees (kernel, virtiofsd builds)
# - CARGO_TARGET_DIR: one per checkout, /cache/project/main/cargo for the
#   main checkout and /cache/project/worktree-<hash>/cargo for a worktree

[[ "${IS_SANDBOX:-}" == 1 ]] || return 0

export AIRLOCK_DEV_CACHE_DIR=/cache/project

_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd -P)"
_git_dir="$(git -C "$_root" rev-parse --path-format=absolute --git-dir 2>/dev/null)" || _git_dir=
_common_dir="$(git -C "$_root" rev-parse --path-format=absolute --git-common-dir 2>/dev/null)" || _common_dir=
if [[ "$_git_dir" == "$_common_dir" ]]; then
  _name=main
else
  _name="worktree-$(printf '%s' "$_root" | sha256sum | cut -c1-12)"
  # The worktree-cleanup daemon (airlock.toml) reads this to find the
  # worktree and deletes the directory once the worktree is removed
  if [[ ! -f "$AIRLOCK_DEV_CACHE_DIR/$_name/.worktree" ]]; then
    mkdir -p "$AIRLOCK_DEV_CACHE_DIR/$_name" &&
      printf '%s\n' "$_root" >"$AIRLOCK_DEV_CACHE_DIR/$_name/.worktree"
  fi
fi
export CARGO_TARGET_DIR="$AIRLOCK_DEV_CACHE_DIR/$_name/cargo"

unset _root _git_dir _common_dir _name

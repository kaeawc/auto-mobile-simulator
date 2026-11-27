#!/usr/bin/env bash
# The script and docs linters, each reading its config at the repository root:
#
#   scripts/lint.sh                 # every linter installed here; a missing one prints a skip line
#   scripts/lint.sh --require       # every linter; a missing one fails (CI)
#   scripts/lint.sh shfmt typos     # just these
#
# Each tool, and what it reads:
#   - shellcheck: every shell script in the tree (.shellcheckrc)
#   - shfmt: the same scripts, as a diff against the style .editorconfig sets
#   - lychee: the Markdown's links to files in the tree and their anchors, offline (lychee.toml)
#   - typos: code and docs (_typos.toml)
#   - actionlint: the workflows, with shellcheck over their `run:` blocks when it is installed
#   - yamllint: every YAML file (.yamllint.yml)
#
# Shell scripts are the tracked and new files shfmt takes for shell (*.sh, a sh or bash
# shebang), minus those .editorconfig marks `ignore = true`, so listing them needs shfmt.
set -uo pipefail
cd "$(dirname "$0")/.." || exit 1

all=(shellcheck shfmt lychee typos actionlint yamllint)
require=0
tools=()
for a in "$@"; do
  case $a in
    --require) require=1 ;;
    shellcheck | shfmt | lychee | typos | actionlint | yamllint) tools+=("$a") ;;
    *)
      echo "usage: scripts/lint.sh [--require] [${all[*]}]..." >&2
      exit 2
      ;;
  esac
done
[[ ${#tools[@]} -gt 0 ]] || tools=("${all[@]}")

# Tracked and new (unignored) files that exist; shfmt keeps the shell ones.
shell_files() {
  git ls-files -z --cached --others --exclude-standard | while IFS= read -r -d '' f; do
    [[ -f $f ]] && printf '%s\0' "$f"
  done | xargs -0 shfmt --apply-ignore -f
}

failed=0
for tool in "${tools[@]}"; do
  needs=("$tool")
  [[ $tool == shellcheck ]] && needs+=(shfmt)
  missing=
  for bin in "${needs[@]}"; do
    command -v "$bin" >/dev/null 2>&1 || missing+="$bin "
  done
  if [[ -n $missing ]]; then
    if [[ $require == 1 ]]; then
      echo "lint: $tool: ${missing}not installed" >&2
      failed=1
    else
      echo "lint: $tool: skipped (${missing}not installed)"
    fi
    continue
  fi
  case $tool in
    shellcheck) shell_files | xargs shellcheck ;;
    shfmt) shell_files | xargs shfmt -d ;;
    lychee) lychee . ;;
    typos) typos ;;
    actionlint) actionlint ;;
    yamllint) yamllint --strict . ;;
    *) false ;;
  esac
  status=$?
  if [[ $status == 0 ]]; then
    echo "lint: $tool: ok"
  else
    echo "lint: $tool: FAILED" >&2
    failed=1
  fi
done
exit "$failed"

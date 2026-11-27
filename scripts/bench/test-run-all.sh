#!/usr/bin/env bash
# Test for run-all.sh's handling of an interrupted benchmark, with fake child scripts (no device):
# a child exiting 130 (SIGINT) or 143 (SIGTERM) stops the whole run, which exits with that status
# and never starts the next child; a failing child (exit 1) and a skipped one (exit 3) don't stop it.
#
#   scripts/bench/test-run-all.sh
set -uo pipefail
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d "${TMPDIR:-/tmp}/test-run-all.XXXXXX")
trap 'rm -rf "$work"' EXIT
fails=0

# shellcheck disable=SC2016 # the child's $0 expands when it runs
mkchild() { # name exit-code
  printf '#!/usr/bin/env bash\necho "$0" >> "%s/ran"\nexit %s\n' "$work" "$2" > "$work/$1.sh"
  chmod +x "$work/$1.sh"
}
check() { # description, condition result
  if [ "$2" = ok ]; then echo "ok   $1"; else echo "FAIL $1"; fails=$((fails + 1)); fi
}
run() { # registry lines on stdin; sets rc and out
  : > "$work/ran"
  cat > "$work/registry"
  out=$(RUN_ALL_REGISTRY="$work/registry" RUN_ALL_DIR="$work" "$here/run-all.sh" 2>&1 < /dev/null); rc=$?
}
ran() { sed 's|.*/||' "$work/ran" | tr '\n' ' '; }

mkchild a 0; mkchild b 130; mkchild c 0; mkchild d 143; mkchild f 1; mkchild s 3

run << 'EOF'
a|host|a.sh|x
b|host|b.sh|x
c|host|c.sh|x
EOF
check "SIGINT exit 130 stops the run with 130" "$([ "$rc" = 130 ] && echo ok)"
check "the child after the interrupted one never ran" "$([ "$(ran)" = "a.sh b.sh " ] && echo ok)"
check "the summary names the stop" "$(printf '%s' "$out" | grep -q 'STOPPED  b' && echo ok)"

run << 'EOF'
a|host|a.sh|x
d|host|d.sh|x
c|host|c.sh|x
EOF
check "SIGTERM exit 143 stops the run with 143" "$([ "$rc" = 143 ] && echo ok)"
check "nothing ran after it" "$([ "$(ran)" = "a.sh d.sh " ] && echo ok)"

run << 'EOF'
f|host|f.sh|x
s|host|s.sh|x
c|host|c.sh|x
EOF
check "a failure and a skip don't stop the run" "$([ "$(ran)" = "f.sh s.sh c.sh " ] && echo ok)"
check "a failure makes the run exit non-zero" "$([ "$rc" = 1 ] && echo ok)"

run << 'EOF'
a|host|a.sh|x
c|host|c.sh|x
EOF
check "all-ok exits 0 and runs everything" "$([ "$rc" = 0 ] && [ "$(ran)" = "a.sh c.sh " ] && echo ok)"

[ "$fails" = 0 ] || { echo "$fails failed"; exit 1; }
echo "all passed"

#!/usr/bin/env sh
# The MVC boundaries (CONTRIBUTING.md): no toolkit or raw threads in controllers, no model in views.
# Test code is left out: a test may start a thread to play a 1990s server.
set -u
cd "$(dirname "$0")/.." || exit 2
fail=0

report() {
    echo "boundary: $1"
    echo "$2" | sed 's/^/  /'
    fail=1
}

# Lines matching $1 in the given files, stopping at each file's `#[cfg(test)]`.
search() {
    pattern=$1
    shift
    for f in "$@"; do
        awk -v pat="$pattern" -v file="$f" '/#\[cfg\(test\)\]/ { exit } $0 ~ pat { print file ":" NR ": " $0 }' "$f"
    done
}

rs() {
    find "$@" -name '*.rs' -not -path '*/target/*' -not -path '*/tests/*' 2>/dev/null
}

hits=$(search '(windows|objc2|gtk4?|ratatui|crossterm)::' $(rs src/controller src/model))
[ -n "$hits" ] && report "toolkit code below the views" "$hits"

hits=$(search 'thread::spawn' $(rs src crates frontends) | grep -v '^src/model/job.rs:' | grep -v '^src/model/worker.rs:')
[ -n "$hits" ] && report "thread::spawn outside model::job / model::worker" "$hits"

hits=$(search 'legcon::model|lc_core::|use lc_' $(rs frontends src/cli))
[ -n "$hits" ] && report "a view reaching past the controllers" "$hits"

hits=$(search 'crate::model' $(rs src/cli))
[ -n "$hits" ] && report "the CLI reaching past the controllers" "$hits"

[ "$fail" -eq 0 ] && echo "boundaries OK"
exit "$fail"

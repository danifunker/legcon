#!/usr/bin/env sh
# The MVC boundaries (CONTRIBUTING.md): no toolkit or raw threads in controllers, no model in views.
set -u
cd "$(dirname "$0")/.." || exit 2
fail=0

report() {
    echo "boundary: $1"
    echo "$2" | sed 's/^/  /'
    fail=1
}

hits=$(grep -rnE '\b(windows|objc2|gtk4?|ratatui|crossterm)::' src/controller src/model 2>/dev/null)
[ -n "$hits" ] && report "toolkit code below the views" "$hits"

hits=$(grep -rn 'thread::spawn' src crates frontends --include='*.rs' 2>/dev/null \
    | grep -v '^src/model/job.rs:' | grep -v '^src/model/worker.rs:')
[ -n "$hits" ] && report "thread::spawn outside model::job / model::worker" "$hits"

hits=$(grep -rnE 'legcon::model|lc_core::|use lc_' frontends src/cli --include='*.rs' 2>/dev/null)
[ -n "$hits" ] && report "a view reaching past the controllers" "$hits"

hits=$(grep -rnE 'crate::model' src/cli --include='*.rs' 2>/dev/null)
[ -n "$hits" ] && report "the CLI reaching past the controllers" "$hits"

[ "$fail" -eq 0 ] && echo "boundaries OK"
exit "$fail"

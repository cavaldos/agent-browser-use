#!/usr/bin/env bash
# Compare two harness outputs: diff.sh <dirA> <dirB>  (exit 0 = identical, 1 = differences)
# Server logs (logs/) are ignored: they hold ports, pids and times.
A=${1:?usage: diff.sh <dirA> <dirB>}
B=${2:?usage: diff.sh <dirA> <dirB>}
diff -ru --exclude=logs "$A" "$B" && echo "parity: no differences"

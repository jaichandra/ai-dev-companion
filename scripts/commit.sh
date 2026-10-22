#!/bin/sh
# Commits everything, but only if the gate (scripts/check.sh) passes.
# Usage: scripts/commit.sh "message"
cd "$(dirname "$0")/.." || exit 1
if scripts/check.sh > /tmp/adc-check.out 2>&1; then
  git add -A && git commit -q -m "$1" && echo "committed: $1" && grep -E "test result: ok\. [0-9]{3,} passed" /tmp/adc-check.out | head -1
else
  echo "GATE FAILED - nothing committed"
  grep -E "^(error|warning)|FAILED|panicked" -A5 /tmp/adc-check.out | head -30
  exit 1
fi

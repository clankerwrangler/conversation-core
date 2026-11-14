#!/bin/sh
# Verify expected safety and temporal violations are distinguished from TLC errors.
set -eu
here=$(CDPATH= cd -- "$(dirname "$0")" && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT HUP INT TERM
mkdir "$tmp/bin" "$tmp/spec"
cp "$here/check.sh" "$tmp/spec/check.sh"
cat >"$tmp/bin/java" <<'JAVA'
#!/bin/sh
case "$*" in
  *Good.cfg*) echo "No error has been found"; exit 0 ;;
  *Invariant.Broken.cfg*) echo "Error: Invariant Safe is violated."; exit 12 ;;
  *Temporal.Broken.cfg*) echo "Error: Temporal properties were violated."; exit 13 ;;
  *Failure.Broken.cfg*) echo "Error: Invariant Safe is violated."; exit 1 ;;
  *) exit 1 ;;
esac
JAVA
chmod +x "$tmp/bin/java"
touch "$tmp/spec/Good.cfg" "$tmp/spec/Invariant.Broken.cfg" "$tmp/spec/Temporal.Broken.cfg"
PATH="$tmp/bin:$PATH" sh "$tmp/spec/check.sh" >"$tmp/out"
touch "$tmp/spec/Failure.Broken.cfg"
if PATH="$tmp/bin:$PATH" sh "$tmp/spec/check.sh" >"$tmp/out"; then
  echo "check.sh accepted a TLC error" >&2
  exit 1
fi
grep -q 'FAIL Failure.Broken.cfg error (expected violation)' "$tmp/out"
echo "check.sh distinguishes expected violations from TLC errors"

#!/bin/sh
# Model-check every specification/*.cfg and compare with its expected outcome:
# a config whose name has a second part (Model.Broken.cfg) is a broken control and
# must report a violation; every other config must pass.
#   TLA2TOOLS=/path/to/tla2tools.jar specification/check.sh
cd "$(dirname "$0")" || exit 1
jar=${TLA2TOOLS:-/opt/tla/tla2tools-1.7.4.jar}
status=0
for cfg in *.cfg; do
  model=${cfg%%.*}
  if [ "$cfg" = "$model.cfg" ]; then want=pass; else want=violation; fi
  out=$(java -XX:+UseParallelGC -cp "$jar" tlc2.TLC -workers auto -config "$cfg" "$model.tla" 2>&1)
  if echo "$out" | grep -q "No error has been found"; then got=pass
  elif echo "$out" | grep -q "is violated"; then got=violation
  else got=error; fi
  if [ "$got" = "$want" ]; then echo "ok   $cfg $got"; else echo "FAIL $cfg $got (expected $want)"; echo "$out" | tail -20; status=1; fi
done
exit $status

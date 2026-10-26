#!/usr/bin/env bash
# Copyright (c) Microsoft Corporation.
# Licensed under the MIT License.
#
# Model-check the orchestration lease specification with TLC and compare each
# result with expected.tsv.
#
# Usage: ./run.sh [config ...]
#   With no arguments, runs every configuration in expected.tsv.
#
# Needs Java 17 or newer. Downloads tla2tools.jar v1.7.4 (TLC 2.19) on first
# use and checks its SHA-256, unless TLA2TOOLS_JAR points to a local copy.
# Exits non-zero if any result differs from expected.tsv.

set -uo pipefail
cd "$(dirname "$0")"

JAR_URL="https://github.com/tlaplus/tlaplus/releases/download/v1.7.4/tla2tools.jar"
JAR_SHA256="936a262061c914694dfd669a543be24573c45d5aa0ff20a8b96b23d01e050e88"
JAR="${TLA2TOOLS_JAR:-tla2tools.jar}"

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        shasum -a 256 "$1" | cut -d' ' -f1
    fi
}

if [ ! -f "$JAR" ]; then
    curl -fsSL -o "$JAR" "$JAR_URL" || { echo "failed to download $JAR_URL" >&2; exit 2; }
fi
if [ "$(sha256 "$JAR")" != "$JAR_SHA256" ]; then
    echo "$JAR does not match the expected SHA-256 $JAR_SHA256" >&2
    exit 2
fi

if [ "$#" -eq 0 ]; then
    set -- $(awk -F'\t' '!/^#/ { print $1 }' expected.tsv)
fi

mkdir -p logs states
failures=0
for name in "$@"; do
    row="$(awk -F'\t' -v n="$name" '$1 == n' expected.tsv)"
    if [ -z "$row" ]; then
        echo "UNKNOWN  $name (not in expected.tsv)"
        failures=$((failures + 1))
        continue
    fi
    IFS=$'\t' read -r _ module heap expected distinct depth <<< "$row"

    start=$(date +%s)
    java -Xmx"$heap" -XX:+UseParallelGC -cp "$JAR" tlc2.TLC \
        -workers 1 -config "configs/$name.cfg" -metadir "states/$name" \
        "$module" > "logs/$name.log" 2>&1
    seconds=$(( $(date +%s) - start ))

    if grep -q 'Model checking completed. No error has been found.' "logs/$name.log"; then
        actual="holds"
    elif violated=$(grep -oE 'Invariant [A-Za-z]+ is violated' "logs/$name.log" | head -1 | cut -d' ' -f2) \
         && [ -n "$violated" ]; then
        actual="violates:$violated"
    else
        actual="error"
    fi
    found=$(grep -oE '[0-9]+ distinct states found' "logs/$name.log" | tail -1 | cut -d' ' -f1)

    status="PASS"
    if [ "$actual" != "$expected" ]; then
        status="FAIL"
    elif [ "$actual" = "holds" ] && [ "$found" != "$distinct" ]; then
        status="FAIL"
    fi
    [ "$status" = "FAIL" ] && failures=$((failures + 1))
    printf '%s  %-28s %-38s distinct=%s (expected %s)  %ss\n' \
        "$status" "$name" "$actual" "${found:-?}" "$distinct" "$seconds"
done

if [ "$failures" -gt 0 ]; then
    echo "$failures configuration(s) did not match expected.tsv; see logs/" >&2
    exit 1
fi
echo "All configurations match expected.tsv."

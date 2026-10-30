#!/usr/bin/env bash
# Turns a results directory from run.sh into markdown tables.
# Usage: summarize.sh <results dir>
set -euo pipefail
dir=$1

echo "### Steady state (etcd's benchmark tool, 256 clients over 16 connections)"
echo
echo "| System | Workload | Requests/s | p50 | p99 |"
echo "|---|---|---|---|---|"
for f in "$dir"/*-steady.txt; do
  [ -e "$f" ] || continue
  system=$(basename "$f" -steady.txt)
  awk -v sys="$system" '
    # The progress bar of the tool ends without a newline, so a heading may follow it.
    match($0, /## .*/) { workload = substr($0, RSTART + 3) }
    /Requests\/sec:/ { rps = $2 }
    /  50% in / { p50 = $3 }
    /  99% in / {
      printf "| %s | %s | %.0f | %.1f ms | %.1f ms |\n", sys, workload, rps, p50 * 1000, $3 * 1000
    }' "$f"
done
echo
echo "### Faults (open-loop load on the followers; latency from each request's due time)"
echo
echo "| System | Scenario | p50 | p99 | max | Errors | Unanswered | Longest gap | Degraded |"
echo "|---|---|---|---|---|---|---|---|---|"
for f in "$dir"/*.txt; do
  case $f in *-steady.txt | */env.txt) continue ;; esac
  name=$(basename "$f" .txt)
  system=${name%-*}
  case $name in *-slow-disk) system=${name%-slow-disk}; scenario=slow-disk ;;
    *-far-leader) system=${name%-far-leader}; scenario=far-leader ;;
    *) scenario=${name##*-} ;; esac
  awk -v sys="$system" -v scenario="$scenario" '
    function field(s, k,   i, n, kv) { n = split(s, kv, " "); for (i = 1; i <= n; i++) if (index(kv[i], k "=") == 1) return substr(kv[i], length(k) + 2); return "" }
    function ms(v) { sub(/us$/, "", v); return sprintf("%.1f ms", v / 1000) }
    NR == 1 { line = $0 }
    NR == 2 {
      printf "| %s | %s | %s | %s | %s | %s | %s | %s | %s |\n", sys, scenario, ms(field(line, "p50")),
        ms(field(line, "p99")), ms(field(line, "max")), field(line, "errors"), field(line, "unanswered"),
        field($0, "longest_gap"), field($0, "degraded")
    }' "$f"
done

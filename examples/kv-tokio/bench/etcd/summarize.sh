#!/usr/bin/env bash
# Turns a results directory from run.sh into markdown tables (supporting both single-run
# legacy files and multi-repeat files with median and range, plus disk probe regimes).
# Usage: summarize.sh <results dir>
# Environment: DISK_SLOW_MS (probe fsync p50 in ms from which a run counts as in the slow
#   disk regime, default 6).
set -euo pipefail
dir=$1
slow_ms=${DISK_SLOW_MS:-6}
tmpfs=0
[ -f "$dir/env.txt" ] && grep -qx 'data: tmpfs' "$dir/env.txt" && tmpfs=1

systems="quepaxa quepaxa-auto etcd etcd-tuned openraft openraft-tuned"

disk_p50() {
  local disk="$1-disk.txt"
  if [ ! -s "$disk" ]; then echo "-"; return; fi
  sort -n <(awk '{ print $2 }' "$disk") | awk '
    { v[NR] = $1 }
    END { if (NR) printf "%.1f", v[int((NR - 1) * 0.5 + 0.5) + 1] / 1000; else printf "-" }'
}

regime() {
  if [ "$tmpfs" = 1 ]; then echo tmpfs; return; fi
  awk -v slow="$slow_ms" '
    $NF != "-" { if ($NF + 0 >= slow) s = 1; else f = 1 }
    END { print s && f ? "mixed" : s ? "slow" : f ? "fast" : "-" }'
}

# Reads rows of numbers (one run per row); prints each column as "median (min–max)".
combine() {
  awk '
    { for (c = 1; c <= NF; c++) col[c, NR] = $c; if (NF > nf) nf = NF }
    function med(c,   i, j, t, a, m) {
      for (i = 1; i <= NR; i++) a[i] = col[c, i] + 0
      for (i = 1; i <= NR; i++) for (j = i + 1; j <= NR; j++) if (a[j] < a[i]) { t = a[i]; a[i] = a[j]; a[j] = t }
      if (a[1] == a[NR]) return fmt(a[1])
      m = NR % 2 ? a[(NR + 1) / 2] : (a[NR / 2] + a[NR / 2 + 1]) / 2
      return fmt(m) " (" fmt(a[1]) "–" fmt(a[NR]) ")"
    }
    function fmt(x) { return x == int(x) ? sprintf("%d", x) : sprintf("%.1f", x) }
    function word(c,   i, n, w, best) {
      for (i = 1; i <= NR; i++) n[col[c, i]]++
      for (w in n) if (best == "" || n[w] > n[best]) best = w
      return best
    }
    END {
      for (c = 1; c <= nf; c++) printf " | %s", (col[c, 1] ~ /^[0-9.]+$/ ? med(c) : word(c))
      print " |"
    }'
}

# Lists matching run files for <system>-<case> (either <sys>-<case>.txt or <sys>-<case>-<run>.txt).
run_files() {
  local base="$dir/$1-$2"
  shopt -s nullglob
  local files=("$base"-[0-9]*.txt)
  [ -e "$base.txt" ] && files=("$base.txt" "${files[@]}")
  shopt -u nullglob
  local f
  for f in "${files[@]}"; do
    case $f in *-disk.txt) continue ;; esac
    echo "$f"
  done
}

# --- Steady state ---
has_steady=0
for sys in $systems; do [ -n "$(run_files "$sys" steady)" ] && has_steady=1; done
if [ "$has_steady" = 1 ]; then
  echo "### Steady state (etcd's benchmark tool, 256 clients over 16 connections)"
  echo
  echo "| System | Workload | Requests/s | p50 (ms) | p99 (ms) |"
  echo "|---|---|---|---|---|"
  for sys in $systems; do
    mapfile -t files < <(run_files "$sys" steady)
    [ ${#files[@]} -gt 0 ] || continue
    mapfile -t workloads < <(awk 'match($0, /## .*/) { w = substr($0, RSTART + 3); if (!seen[w]++) print w }' "${files[0]}")
    for w in "${workloads[@]}"; do
      rows=$(for f in "${files[@]}"; do
        awk -v want="$w" '
          match($0, /## .*/) { cur = substr($0, RSTART + 3) }
          cur == want && /Requests\/sec:/ { rps = $2 }
          cur == want && /  50% in / { p50 = $3 * 1000 }
          cur == want && /  99% in / { printf "%.0f %.1f %.1f\n", rps, p50, $3 * 1000 }' "$f"
      done)
      [ -n "$rows" ] || continue
      echo "| $sys | $w$(combine <<< "$rows")"
    done
  done
  echo
fi

# --- Load curve ---
load_rates=$(find "$dir" -maxdepth 1 -name '*-load-*.txt' ! -name '*-disk.txt' -printf '%f\n' |
  sed -n 's/^.*-load-\([0-9][0-9]*\)\(-[0-9][0-9]*\)\?\.txt$/\1/p' | sort -Vu)
if [ -n "$load_rates" ]; then
  echo "### Load curve (open loop, 100% puts, all members; latency in ms)"
  echo
  echo "| System | Offered/s | Completed/s | p50 | p99 | max | Errors | Unanswered | Disk fsync p50 | Disk regime |"
  echo "|---|---|---|---|---|---|---|---|---|---|"
  for sys in $systems; do
    for r in $load_rates; do
      mapfile -t files < <(run_files "$sys" "load-$r")
      [ ${#files[@]} -gt 0 ] || continue
      rows=$(for f in "${files[@]}"; do
        base=${f%.txt}
        line=$(awk '
          function field(k,   i) { for (i = 1; i <= NF; i++) if (index($i, k "=") == 1) return substr($i, length(k) + 2); return "" }
          function ms(v) { sub(/us$/, "", v); return v / 1000 }
          NR == 1 {
            tp = field("throughput"); sub(/\/s$/, "", tp)
            printf "%.0f %.1f %.1f %.1f %d %d", tp + 0, ms(field("p50")), ms(field("p99")), ms(field("max")), field("errors"), field("unanswered")
          }' "$f")
        echo "$line $(disk_p50 "$base")"
      done)
      reg=$(regime <<< "$rows")
      echo "| $sys | $r$(combine <<< "$rows") $reg |"
    done
  done
  echo
fi

# --- Mixed (read-heavy) ---
has_mixed=0
for sys in $systems; do [ -n "$(run_files "$sys" mixed)" ] && has_mixed=1; done
if [ "$has_mixed" = 1 ]; then
  echo "### Read-heavy (open loop, 90% linearizable ranges and 10% puts, all members; latency in ms)"
  echo
  echo "| System | Offered | Completed/s | p50 | p99 | max | Errors | Unanswered | Disk fsync p50 | Disk regime |"
  echo "|---|---|---|---|---|---|---|---|---|---|"
  for sys in $systems; do
    mapfile -t files < <(run_files "$sys" mixed)
    [ ${#files[@]} -gt 0 ] || continue
    rows=$(for f in "${files[@]}"; do
      base=${f%.txt}
      line=$(awk '
        function field(k,   i) { for (i = 1; i <= NF; i++) if (index($i, k "=") == 1) return substr($i, length(k) + 2); return "" }
        function ms(v) { sub(/us$/, "", v); return v / 1000 }
        NR == 1 {
          tp = field("throughput"); sub(/\/s$/, "", tp)
          printf "%s %.0f %.1f %.1f %.1f %d %d", field("offered"), tp + 0, ms(field("p50")), ms(field("p99")), ms(field("max")), field("errors"), field("unanswered")
        }' "$f")
      echo "$line $(disk_p50 "$base")"
    done)
    reg=$(regime <<< "$rows")
    echo "| $sys$(combine <<< "$rows") $reg |"
  done
  echo
fi

# --- Faults ---
fault_scenarios=(crash partition slow-disk stuck-disk far-leader)
has_fault=0
for sys in $systems; do
  for sc in "${fault_scenarios[@]}"; do
    [ -n "$(run_files "$sys" "$sc")" ] && has_fault=1
  done
done
if [ "$has_fault" = 1 ]; then
  echo "### Faults (open-loop load on the followers; latency from each request's due time, ms)"
  echo
  echo "| System | Scenario | p50 | p99 | max | Errors | Unanswered | Longest gap | Degraded | Disk fsync p50 | Disk regime |"
  echo "|---|---|---|---|---|---|---|---|---|---|---|"
  for sys in $systems; do
    for sc in "${fault_scenarios[@]}"; do
      mapfile -t files < <(run_files "$sys" "$sc")
      [ ${#files[@]} -gt 0 ] || continue
      rows=$(for f in "${files[@]}"; do
        base=${f%.txt}
        line=$(awk '
          function field(s, k,   i, n, kv) { n = split(s, kv, " "); for (i = 1; i <= n; i++) if (index(kv[i], k "=") == 1) return substr(kv[i], length(k) + 2); return "" }
          function ms(v) { sub(/us$/, "", v); return v / 1000 }
          NR == 1 { l1 = $0 }
          NR == 2 {
            g = field($0, "longest_gap"); sub(/ms$/, "", g)
            d = field($0, "degraded"); sub(/ms$/, "", d)
            printf "%.1f %.1f %.1f %d %d %d %d", ms(field(l1, "p50")), ms(field(l1, "p99")), ms(field(l1, "max")),
              field(l1, "errors"), field(l1, "unanswered"), g + 0, d + 0
          }' "$f")
        echo "$line $(disk_p50 "$base")"
      done)
      reg=$(regime <<< "$rows")
      echo "| $sys | $sc$(combine <<< "$rows") $reg |"
    done
  done
fi

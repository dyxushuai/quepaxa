#!/usr/bin/env bash
# Turns a results directory from wan.sh into markdown tables: one row per system and case
# over all regions, then one per region, each value the median over runs with the range.
# Usage: summarize-wan.sh <results dir>
set -euo pipefail
dir=$1

# Prints "<key> <value>..." for one run, over all regions: p50 p99 max (ms, from the merged
# samples), errors, unanswered, longest gap, degraded (worst region), leader after.
run_row() {
  local name=$1
  sort -n <(awk -F, 'FNR > 1 && $3 == "Ok" { print $2 - $1 }' "$dir/$name"-r?.csv) | awk '
    { v[NR] = $1 }
    function pct(p) { return NR ? v[int((NR - 1) * p + 0.5) + 1] / 1000 : 0 }
    END { printf "%.1f %.1f %.1f", pct(0.5), pct(0.99), NR ? v[NR] / 1000 : 0 }'
  awk '
    function field(k,   i) { for (i = 1; i <= NF; i++) if (index($i, k "=") == 1) return substr($i, length(k) + 2); return "" }
    /^offered/ { e += field("errors"); u += field("unanswered") }
    /^fault_at/ { g = field("longest_gap") + 0; d = field("degraded") + 0; if (g > mg) mg = g; if (d > md) md = d }
    END { printf " %d %d %d %d", e, u, mg, md }' "$dir/$name"-r?.txt
  echo " $(sed -n 's/^leader_after=//p' "$dir/$name-leader.txt" 2>/dev/null || echo -)"
}

# One region's row for one run, from its generator's output.
region_row() {
  awk '
    function field(k,   i) { for (i = 1; i <= NF; i++) if (index($i, k "=") == 1) return substr($i, length(k) + 2); return "" }
    function ms(v) { sub(/us$/, "", v); return v / 1000 }
    # A region with no completions has no latency to show.
    /^offered/ {
      if (field("completed") + 0 == 0) printf "- - -"
      else printf "%.1f %.1f %.1f", ms(field("p50")), ms(field("p99")), ms(field("max"))
      printf " %d %d", field("errors"), field("unanswered")
    }
    /^fault_at/ { printf " %d %d", field("longest_gap"), field("degraded") }
    END { print "" }' "$1"
}

# Reads rows of numbers (one run per row); prints each column as "median (min–max)", or
# the most frequent word for a non-numeric column.
combine() {
  awk '
    { for (c = 1; c <= NF; c++) col[c, NR] = $c; if (NF > nf) nf = NF }
    function med(c,   i, j, t, a) {
      for (i = 1; i <= NR; i++) a[i] = col[c, i] + 0
      for (i = 1; i <= NR; i++) for (j = i + 1; j <= NR; j++) if (a[j] < a[i]) { t = a[i]; a[i] = a[j]; a[j] = t }
      if (a[1] == a[NR]) return fmt(a[1])
      return fmt(a[int((NR + 1) / 2)]) " (" fmt(a[1]) "–" fmt(a[NR]) ")"
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

# Cases in run order: <system>-<case>, with the run number and region stripped.
cases=$(find "$dir" -name '*-r0.txt' -printf '%f\n' | sed 's/-[0-9]*-r0\.txt$//' | sort -u)
systems="quepaxa etcd etcd-tuned"
regions=$(sed -n 's/^regions: \([^;]*\);.*/\1/p' "$dir/env.txt")

echo "### All regions (median over runs, range in parentheses; latency in ms)"
echo
echo "| System | Case | p50 | p99 | max | Errors | Unanswered | Longest gap (ms) | Degraded (ms) | Leader after |"
echo "|---|---|---|---|---|---|---|---|---|---|"
for system in $systems; do
  for c in $cases; do
    case $c in "$system"-*) ;; *) continue ;; esac
    rest=${c#"$system"-}
    # "etcd-tuned-calm" also starts with "etcd-".
    [ "$system" = etcd ] && [[ $rest == tuned-* ]] && continue
    rows=$(for f in "$dir/$c"-[0-9]*-r0.txt; do run_row "$(basename "$f" -r0.txt)"; done)
    echo "| $system | $rest$(combine <<< "$rows")"
  done
done
echo
echo "### By region"
echo
echo "| System | Case | Region | p50 | p99 | max | Errors | Unanswered | Longest gap (ms) | Degraded (ms) |"
echo "|---|---|---|---|---|---|---|---|---|---|"
for system in $systems; do
  for c in $cases; do
    case $c in "$system"-*) ;; *) continue ;; esac
    rest=${c#"$system"-}
    [ "$system" = etcd ] && [[ $rest == tuned-* ]] && continue
    i=0
    for region in $regions; do
      rows=$(for f in "$dir/$c"-[0-9]*-r"$i".txt; do region_row "$f"; done)
      echo "| $system | $rest | $region$(combine <<< "$rows")"
      i=$((i + 1))
    done
  done
done

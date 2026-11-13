#!/usr/bin/env bash
# Turns a results directory from wan.sh into markdown tables: one row per system and case
# over all regions, then one per region, each value the median over runs with the range.
# Every row also carries the disk probe wan.sh ran alongside (see disk_row) and the disk
# regime it puts the case in.
# Usage: summarize-wan.sh <results dir>
# Environment: DISK_SLOW_MS (probe fsync p50 in ms from which a run counts as in the slow
#   disk regime, default 6).
set -euo pipefail
dir=$1
# #208 measured this host's shared disk in two regimes, probe p50 4.5-4.6 ms and 8.0-8.6 ms,
# with commit p50 about 5.7 and 18 ms; 6 ms lies between them with room on both sides.
slow_ms=${DISK_SLOW_MS:-6}
tmpfs=0
grep -qx 'data: tmpfs' "$dir/env.txt" && tmpfs=1

# One run's disk probe over the same window as its latency: fsync p50 and max in ms, or
# "- -" for a run without probe samples.
disk_row() {
  local from
  from=$(sed -n 's/^measure_from_s=//p' "$dir/$1-leader.txt" 2>/dev/null)
  sort -n <(awk -v from="${from:-0}" '$1 >= from { print $2 }' "$dir/$1-disk.txt" 2>/dev/null) | awk '
    { v[NR] = $1 }
    END { if (NR) printf "%.1f %.1f", v[int((NR - 1) * 0.5 + 0.5) + 1] / 1000, v[NR] / 1000; else printf "- -" }'
}

# Reads run rows whose second-to-last column is the probe's p50; prints the disk regime of
# them all: fast, slow, mixed (runs in both: their medians mix the two), tmpfs (no disk
# under the replicas), or - (no probe).
regime() {
  if [ $tmpfs = 1 ]; then echo tmpfs; return; fi
  awk -v slow="$slow_ms" '
    $(NF - 1) != "-" { if ($(NF - 1) + 0 >= slow) s = 1; else f = 1 }
    END { print s && f ? "mixed" : s ? "slow" : f ? "fast" : "-" }'
}

# Prints one run's values over the regions given (their generators' files): p50 p99 max
# (ms, from the merged samples; for the leader-policy scenarios only requests due once the
# policy settled), errors, unanswered, longest gap, degraded (worst region), leader after,
# and for the policy scenarios leader changes and when the last one was seen (s).
run_row() {
  local name=$1 csvs=() txts=() i from
  for i in $2; do csvs+=("$dir/$name-r$i.csv"); txts+=("$dir/$name-r$i.txt"); done
  from=$(sed -n 's/^measure_from_s=//p' "$dir/$name-leader.txt" 2>/dev/null)
  sort -n <(awk -F, -v from="${from:-0}" 'FNR > 1 && $3 == "Ok" && $1 >= from * 1e6 { print $2 - $1 }' "${csvs[@]}") | awk '
    { v[NR] = $1 }
    function pct(p) { return NR ? v[int((NR - 1) * p + 0.5) + 1] / 1000 : 0 }
    END { printf "%.1f %.1f %.1f", pct(0.5), pct(0.99), NR ? v[NR] / 1000 : 0 }'
  awk '
    function field(k,   i) { for (i = 1; i <= NF; i++) if (index($i, k "=") == 1) return substr($i, length(k) + 2); return "" }
    /^offered/ { e += field("errors"); u += field("unanswered") }
    /^fault_at/ { g = field("longest_gap") + 0; d = field("degraded") + 0; if (g > mg) mg = g; if (d > md) md = d }
    END { printf " %d %d %d %d", e, u, mg, md }' "${txts[@]}"
  echo " $(sed -n 's/^leader_after=//p' "$dir/$name-leader.txt" 2>/dev/null || echo -)" \
    "$(sed -n 's/^leader_changes=//p' "$dir/$name-leader.txt" 2>/dev/null | grep . || echo -)" \
    "$(sed -n 's/^last_change_s=//p' "$dir/$name-leader.txt" 2>/dev/null | grep . || echo -)"
}

# One region's row for one run, from its generator's output; for a leader-policy run, the
# latency only of requests due once the policy settled (none if the region wrote nothing
# then), from its samples.
region_row() {
  local from latency=""
  from=$(sed -n 's/^measure_from_s=//p' "${1%-r*.txt}-leader.txt" 2>/dev/null)
  if [ -n "$from" ]; then
    latency=$(sort -n <(awk -F, -v from="$from" 'FNR > 1 && $3 == "Ok" && $1 >= from * 1e6 { print $2 - $1 }' "${1%.txt}.csv") | awk '
      { v[NR] = $1 }
      function pct(p) { return v[int((NR - 1) * p + 0.5) + 1] / 1000 }
      END { if (NR) printf "%.1f %.1f %.1f", pct(0.5), pct(0.99), v[NR] / 1000; else printf "- - -" }')
  fi
  awk -v latency="$latency" '
    function field(k,   i) { for (i = 1; i <= NF; i++) if (index($i, k "=") == 1) return substr($i, length(k) + 2); return "" }
    function ms(v) { sub(/us$/, "", v); return v / 1000 }
    # A region with no completions has no latency to show.
    /^offered/ {
      if (latency != "") printf "%s", latency
      else if (field("completed") + 0 == 0) printf "- - -"
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

# Cases in run order: <system>-<case>, with the run number and region stripped.
cases=$(find "$dir" -name '*-r0.txt' -printf '%f\n' | sed 's/-[0-9]*-r0\.txt$//' | sort -u)
systems="quepaxa etcd etcd-tuned"
regions=$(sed -n 's/^regions: \([^;]*\);.*/\1/p' "$dir/env.txt")
far=$(sed -n 's/^regions: .*far: \([^,]*\),.*/\1/p' "$dir/env.txt")
all="" others="" i=0
for region in $regions; do
  all+=" $i"
  [ "$region" = "$far" ] || others+=" $i"
  i=$((i + 1))
done

echo "### All regions (median over runs, range in parentheses; latency in ms)"
echo
echo "With the leader's region crashed or cut off, its own clients cannot write at all; those"
echo "rows ($far down) cover the other regions."
echo
echo "\`Disk fsync\` is the probe's p50 and max (ms), one 512-byte append with \`fsync\` a"
echo "second on the replicas' data disk over the same window; a run is in the slow disk regime"
echo "from a p50 of $slow_ms ms. Compare only rows of the same regime: on a shared virtual disk"
echo "it moves every commit latency (docs/benchmarks.md, \"A slow member\")."
echo
echo "| System | Case | p50 | p99 | max | Errors | Unanswered | Longest gap (ms) | Degraded (ms) | Leader after | Leader changes | Last change (s) | Disk fsync p50 | Disk fsync max | Disk regime |"
echo "|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|"
all_regimes=""
declare -A case_regime
for system in $systems; do
  for c in $cases; do
    case $c in "$system"-*) ;; *) continue ;; esac
    rest=${c#"$system"-}
    # "etcd-tuned-calm" also starts with "etcd-".
    [ "$system" = etcd ] && [[ $rest == tuned-* ]] && continue
    case $rest in crash | partition) over=$others ;; *) over=$all ;; esac
    rows=$(for f in "$dir/$c"-[0-9]*-r0.txt; do
      name=$(basename "$f" -r0.txt)
      echo "$(run_row "$name" "$over") $(disk_row "$name")"
    done)
    case_regime[$c]=$(regime <<< "$rows")
    all_regimes+=" ${case_regime[$c]}"
    echo "| $system | $rest$(combine <<< "$rows") ${case_regime[$c]} |"
  done
done
echo
seen=$(tr ' ' '\n' <<< "$all_regimes" | grep -vx -e '' -e - | sort -u | paste -sd' ')
case $seen in
  "") echo "No disk probe in these runs." ;;
  fast | slow | tmpfs) echo "Every case ran in the $seen disk regime." ;;
  *) echo "**The cases ran in different disk regimes ($seen):** compare only rows of the same regime." ;;
esac
echo
echo "### By region"
echo
echo "| System | Case | Region | p50 | p99 | max | Errors | Unanswered | Longest gap (ms) | Degraded (ms) | Disk regime |"
echo "|---|---|---|---|---|---|---|---|---|---|---|"
for system in $systems; do
  for c in $cases; do
    case $c in "$system"-*) ;; *) continue ;; esac
    rest=${c#"$system"-}
    [ "$system" = etcd ] && [[ $rest == tuned-* ]] && continue
    i=0
    for region in $regions; do
      rows=$(for f in "$dir/$c"-[0-9]*-r"$i".txt; do region_row "$f"; done)
      echo "| $system | $rest | $region$(combine <<< "$rows") ${case_regime[$c]} |"
      i=$((i + 1))
    done
  done
done

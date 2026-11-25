#!/usr/bin/env bash
# Turns a results directory from paper-eval.sh into markdown tables: each value the median
# over runs, with the range in parentheses. The kind of experiment is read from the
# directory's name (fig6-lan-*, fig7-*, fig6-wan-*, fig8-*, fig9-*, fig10-*).
# Usage: paper-summarize.sh <results dir>
set -euo pipefail
dir=${1%/}

# Reads rows of numbers (one run per row); prints each column as "median (min-max)".
combine() {
  awk '
    { for (c = 1; c <= NF; c++) col[c, NR] = $c; if (NF > nf) nf = NF }
    function fmt(x) { return x >= 100 || x == int(x) ? sprintf("%d", x + 0.5) : sprintf("%.2f", x) }
    function med(c,   i, j, t, a, m) {
      for (i = 1; i <= NR; i++) a[i] = col[c, i] + 0
      for (i = 1; i <= NR; i++) for (j = i + 1; j <= NR; j++) if (a[j] < a[i]) { t = a[i]; a[i] = a[j]; a[j] = t }
      m = NR % 2 ? a[(NR + 1) / 2] : (a[NR / 2] + a[NR / 2 + 1]) / 2
      return a[1] == a[NR] ? fmt(m) : fmt(m) " (" fmt(a[1]) "-" fmt(a[NR]) ")"
    }
    END { for (c = 1; c <= nf; c++) printf " | %s", (col[c, 1] ~ /^-?[0-9.]+$/ ? med(c) : col[c, 1]); print " |" }'
}

# kv-bench's summary line: throughput (/s), p50 and p99 (ms), and what went missing.
bench_row() {
  awk '
    function field(k,   i) { for (i = 1; i <= NF; i++) if (index($i, k "=") == 1) return substr($i, length(k) + 2); return "" }
    /^offered=/ { printf "%d %.2f %.2f %d\n", field("throughput") + 0, field("p50") / 1000, field("p99") / 1000,
      field("errors") + field("unanswered") + field("missed") }' "$1"
}

# pct_csvs <from s> <to s> <csv...>: p50 and p99 (ms) of the successful requests due in that
# window, merged over the generators' samples.
pct_csvs() {
  local from=$1 to=$2
  shift 2
  awk -F, -v f="$from" -v t="$to" 'FNR > 1 && $3 == "Ok" && $1 >= f * 1e6 && $1 < t * 1e6 { print $2 - $1 }' "$@" |
    sort -n -S 1G | awk '
      { v[NR] = $1 }
      function pct(p) { return NR ? v[int((NR - 1) * p + 0.5) + 1] / 1000 : 0 }
      END { printf "%.2f %.2f", pct(0.5), pct(0.99) }'
}

# cluster_gap_ms <from s> <to s> <csv...>: longest time (ms) in [from, to] with no
# successful completion across all given generators.
cluster_gap_ms() {
  local from=$1 to=$2
  shift 2
  awk -F, -v f="$from" -v t="$to" 'FNR > 1 && $3 == "Ok" && $2 >= f * 1e6 && $2 <= t * 1e6 { print $2 }' "$@" |
    sort -n -S 1G | awk -v f="$from" -v t="$to" '
      BEGIN { prev = f * 1e6; gap = 0 }
      { d = $1 - prev; if (d > gap) gap = d; prev = $1 }
      END { d = t * 1e6 - prev; if (d > gap) gap = d; printf "%.1f", gap / 1000 }'
}

# tput_window <from s> <to s> <csv...>: successful completions per second with done_us in
# [from, to) across all given generators.
tput_window() {
  local from=$1 to=$2
  shift 2
  awk -F, -v f="$from" -v t="$to" '
    FNR > 1 && $3 == "Ok" && $2 >= f * 1e6 && $2 < t * 1e6 { c++ }
    END { printf "%d", (t > f ? c / (t - f) + 0.5 : 0) }' "$@"
}

host() {
  local f
  for f in "$dir/env.txt" "$dir/host.txt"; do [ -f "$f" ] && sed 's/^/    /' "$f"; done
  echo
}

case $(basename "$dir") in
  fig6-lan-*)
    echo "### Fig. 6a-b: LAN, 3 replicas"
    echo
    host
    echo "| Offered | Throughput | p50 (ms) | p99 (ms) | Errors + unanswered + missed |"
    echo "|---|---|---|---|---|"
    for rate in $(find "$dir" -name 'n3-*-*.txt' -printf '%f\n' | cut -d- -f2 | sort -nu); do
      echo "| $rate$(for f in "$dir"/n3-"$rate"-*.txt; do bench_row "$f"; done | combine)"
    done
    ;;
  fig7-*)
    echo "### Fig. 7: LAN saturation throughput (p50 at most 5.8 ms)"
    echo
    host
    echo "| Replicas | Saturation throughput | p50 at it (ms) | p99 at it (ms) |"
    echo "|---|---|---|---|"
    cut -d" " -f1 "$dir/capacity.txt" | sort -u -V | while read -r tag; do
      awk -v t="$tag" '$1 == t' "$dir/capacity.txt" | while read -r _ run best; do
        if [ "$best" -gt 0 ]; then echo "$best $(bench_row "$dir/$tag-$best-$run.txt" | cut -d' ' -f2,3)"; else echo "0 0 0"; fi
      done | { printf '| %s' "${tag#n}"; combine; }
    done
    ;;
  fig6-wan-*)
    echo "### Fig. 6c-d: emulated WAN, 5 replicas, pipeline 10"
    echo
    host
    secs=$(sed -n 's/.*load rates .* for \([0-9]*\)s each/\1/p' "$dir/env.txt")
    echo "| Offered | Throughput | p50 (ms) | p99 (ms) | Errors + unanswered |"
    echo "|---|---|---|---|---|"
    for rate in $(find "$dir" -name 'quepaxa-load-*-r0.txt' -printf '%f\n' | cut -d- -f3 | sort -nu); do
      for f in "$dir"/quepaxa-load-"$rate"-*-r0.txt; do
        name=${f%-r0.txt}
        tput=$(cat "$name"-r?.txt | awk '/^offered=/ { for (i = 1; i <= NF; i++) if ($i ~ /^throughput=/) { v = substr($i, 12); sub(/\/s$/, "", v); s += v } } END { print s + 0 }')
        lost=$(cat "$name"-r?.txt | awk '/^offered=/ { for (i = 1; i <= NF; i++) if ($i ~ /^(errors|unanswered)=/) { split($i, a, "="); s += a[2] } } END { print s + 0 }')
        echo "$tput $(pct_csvs 0 "$secs" "$name"-r?.csv) $lost"
      done | { printf '| %s' "$rate"; combine; }
    done
    ;;
  fig8-*)
    echo "### Fig. 8: emulated WAN, 5 replicas, adversary delaying a changing minority up to 500 ms every 5 s"
    echo
    host
    secs=$(sed -n 's/.*adversary: .* for \([0-9]*\)s/\1/p' "$dir/env.txt")
    echo "| Offered | Throughput | p50 (ms) | p99 (ms) | Errors + unanswered |"
    echo "|---|---|---|---|---|"
    for rate in $(find "$dir" -name 'quepaxa-adversary-*-r0.txt' -printf '%f\n' | cut -d- -f3 | sort -nu); do
      for f in "$dir"/quepaxa-adversary-"$rate"-*-r0.txt; do
        name=${f%-r0.txt}
        tput=$(cat "$name"-r?.txt | awk '/^offered=/ { for (i = 1; i <= NF; i++) if ($i ~ /^throughput=/) { v = substr($i, 12); sub(/\/s$/, "", v); s += v } } END { print s + 0 }')
        lost=$(cat "$name"-r?.txt | awk '/^offered=/ { for (i = 1; i <= NF; i++) if ($i ~ /^(errors|unanswered)=/) { split($i, a, "="); s += a[2] } } END { print s + 0 }')
        echo "$tput $(pct_csvs 0 "$secs" "$name"-r?.csv) $lost"
      done | { printf '| %s' "$rate"; combine; }
    done
    ;;
  fig9-*)
    echo "### Fig. 9: emulated WAN, 5 replicas, leader failure across hedging delays"
    echo
    host
    secs=$(sed -n 's/.*hedge-crash: .* run \([0-9]*\)s.*/\1/p' "$dir/env.txt")
    fault_s=$(sed -n 's/.*hedge-crash: .* crash at \([0-9]*\)s.*/\1/p' "$dir/env.txt")
    best=$(sed -n 's/^regions: .*best: \([^ ]*\).*/\1/p' "$dir/env.txt")
    read -r -a regions <<< "$(sed -n 's/^regions: \([^;]*\);.*/\1/p' "$dir/env.txt")"
    surv=()
    for i in "${!regions[@]}"; do [ "${regions[$i]}" = "$best" ] || surv+=("$i"); done
    pre_from=$((fault_s >= 10 ? 5 : 1))
    echo "Leader ($best) killed with SIGKILL at ${fault_s}s; pre-crash throughput covers 0-${fault_s}s and pre p50 covers ${pre_from}-$((fault_s - 1))s (all regions); post-crash throughput, recovery gaps and post latency cover ${fault_s}-${secs}s on the four surviving regions; errors + unanswered, hedged and egress are whole-run totals on the four surviving regions."
    echo
    echo "| Hedging delay (ms) | Pre-crash (/s) | Post-crash (/s) | Recovery gap (ms) | Worst-region gap (ms) | Pre p50 (ms) | Post p50 (ms) | Post p99 (ms) | Errors + unanswered | Hedged | Egress (MiB) |"
    echo "|---|---|---|---|---|---|---|---|---|---|---|"
    for delay in $(find "$dir" -name 'quepaxa-hedge-crash-*-r0.txt' -printf '%f\n' | sed 's/^quepaxa-hedge-crash-\(.*\)-[0-9]*-r0\.txt$/\1/' | sort -u -V); do
      for f in "$dir"/quepaxa-hedge-crash-"$delay"-[0-9]*-r0.txt; do
        name=${f%-r0.txt}
        surv_csvs=() surv_txts=()
        for i in "${surv[@]}"; do surv_csvs+=("$name-r$i.csv"); surv_txts+=("$name-r$i.txt"); done
        pre_tput=$(tput_window 0 "$fault_s" "$name"-r?.csv)
        post_tput=$(tput_window "$fault_s" "$secs" "${surv_csvs[@]}")
        rec_gap=$(cluster_gap_ms "$fault_s" "$secs" "${surv_csvs[@]}")
        worst_gap=$(awk '/^fault_at/ { for (i = 1; i <= NF; i++) if ($i ~ /^longest_gap=/) { v = substr($i, 13) + 0; if (v > m) m = v } } END { print m + 0 }' "${surv_txts[@]}")
        read -r pre_p50 _ <<< "$(pct_csvs "$pre_from" $((fault_s - 1)) "$name"-r?.csv)"
        read -r post_p50 post_p99 <<< "$(pct_csvs "$fault_s" "$secs" "${surv_csvs[@]}")"
        lost=$(awk '/^offered=/ { for (i = 1; i <= NF; i++) if ($i ~ /^(errors|unanswered)=/) { split($i, a, "="); s += a[2] } } END { print s + 0 }' "${surv_txts[@]}")
        read -r hedged egress <<< "$(awk '
          {
            if (match($0, /hedged: [0-9]+/)) h += substr($0, RSTART + 8, RLENGTH - 8)
            if (match($0, /EGRESS [^ ]+/)) {
              eg = substr($0, RSTART + 7, RLENGTH - 7)
              k = split(eg, a, ",")
              for (i = 1; i <= k; i++) { split(a[i], kv, "="); b += kv[2] }
            }
          }
          END { printf "%d %.1f", h + 0, b / 1048576 }' "$name-stats.txt")"
        echo "$pre_tput $post_tput $rec_gap $worst_gap $pre_p50 $post_p50 $post_p99 $lost $hedged $egress"
      done | { printf '| %s' "$delay"; combine; }
    done
    ;;
  fig10-*)
    echo "### Fig. 10: convergence to the best leader, LAN, 5 replicas of different sizes"
    echo
    host
    for policy in on off; do
      d=$dir/placement-$policy
      [ -d "$d" ] || continue
      sed -n "s/^\\(ladder quotas\\|policy\\|rate\\|cpus\\|timers\\):/    \\1:/p" "$d/env.txt"
      read -r -a regions <<< "$(sed -n 's/^regions: \([^;]*\);.*/\1/p' "$d/env.txt")"
      read -r -a quotas <<< "$(sed -n 's/^ladder quotas: \([0-9. ]*\) CPUs.*/\1/p' "$d/env.txt")"
      strongest=$(for i in "${!quotas[@]}"; do echo "${quotas[$i]} ${regions[$i]}"; done | sort -g -k1,1 | tail -1 | cut -d' ' -f2)
      from=$(sed -n 's/^measure_from_s=//p' "$d"/quepaxa-ladder-1-leader.txt)
      secs=$(sed -n 's/.*policy runs \([0-9]*\)s.*/\1/p' "$d/env.txt")
      echo
      echo "Placement $policy; the strongest member is $strongest. Leader samples every second; \`At best\`"
      echo "is when the lead reached the strongest member for good (- if it never did)."
      echo
      echo "| Run | Leader changes | At best (s) | Final leader | p50 0-10 s | p50 from ${from} s | p99 from ${from} s |"
      echo "|---|---|---|---|---|---|---|"
      for f in "$d"/quepaxa-ladder-*-r0.txt; do
        name=${f%-r0.txt}
        run=${name##*-}
        changes=$(sed -n 's/^leader_changes=//p' "$name-leader.txt")
        final=$(tail -1 "$name-leaders.txt" | cut -d' ' -f2)
        at=$(awk -v best="$strongest" '$2 != best { at = "" } $2 == best && at == "" { at = $1 } END { print at == "" ? "-" : at }' "$name-leaders.txt")
        read -r early _ <<< "$(pct_csvs 0 10 "$name"-r?.csv)"
        echo "| $run | $changes | $at | $final | $early | $(pct_csvs "$from" "$secs" "$name"-r?.csv | sed 's/ / | /') |"
      done
      echo
      echo "Median latency over time (ms; each generator's p50 per 250 ms interval, averaged over the"
      echo "generators and the interval's 10 s window, then the median over runs):"
      echo
      echo "| Window (s) | p50 |"
      echo "|---|---|"
      for f in "$d"/quepaxa-ladder-*-r0.txt; do
        cat "${f%-r0.txt}"-r?.txt | awk -F, '/^[0-9]+,/ && $2 > 0 { w = int($1 / 10000); s[w] += $4; c[w]++ } END { for (w in s) print w, s[w] / c[w] / 1000 }'
      done | sort -k1,1n -k2,2g | awk '
        function emit() { m = n % 2 ? v[(n + 1) / 2] : (v[n / 2] + v[n / 2 + 1]) / 2; printf "| %d-%d | %.2f |\n", w * 10, w * 10 + 10, m; n = 0 }
        NR > 1 && $1 != w { emit() }
        { w = $1; v[++n] = $2 }
        END { if (n) emit() }'
      echo
    done
    ;;
  *) echo "unknown results directory $dir" >&2; exit 2 ;;
esac

#!/bin/zsh
# Inventory linked git worktrees and their disk cost. Usage: zsh scripts/worktree-inventory.sh <scratch-dir>
S=$1
find $HOME /private/tmp -maxdepth 6 \( -path $HOME/Library -o -path $HOME/.Trash -o -name node_modules -o -name target -o -name .next \) -prune -o -name .git -type f -print 2>/dev/null | sort -u > $S/wt.txt
: > $S/wt.tsv
while read g; do
  d=${g%/.git}
  repo=$(sed -n 's|^gitdir: ||p' "$g" | sed 's|/\.git/worktrees/.*||')
  tot=$(du -sk "$d" 2>/dev/null | cut -f1)
  b=0; for x in target .next dist build out .turbo .zumith-studio .zenith-studio; do [ -e "$d/$x" ] && b=$((b + $(du -sk "$d/$x" 2>/dev/null | cut -f1))); done
  p=0; for x in node_modules .venv vendor .pnpm-store; do [ -e "$d/$x" ] && p=$((p + $(du -sk "$d/$x" 2>/dev/null | cut -f1))); done
  # nested node_modules in monorepos
  n=$(find "$d" -mindepth 2 -maxdepth 4 -name node_modules -type d -not -path '*/node_modules/*' -prune -exec du -sk {} + 2>/dev/null | awk '{s+=$1} END{print s+0}')
  p=$((p + n))
  printf "%s\t%s\t%s\t%s\t%s\n" "$tot" "$b" "$p" "$d" "$repo" >> $S/wt.tsv
done < $S/wt.txt
echo "worktrees: $(wc -l < $S/wt.tsv)"
awk -F'\t' '{t+=$1; b+=$2; p+=$3} END{printf "TOTAL %.1f GB | builds %.1f GB | deps %.1f GB | checkout+other %.1f GB\n", t/1048576, b/1048576, p/1048576, (t-b-p)/1048576}' $S/wt.tsv
echo "=====TOP 15"; sort -t$'\t' -k1 -rn $S/wt.tsv | head -15 | awk -F'\t' '{printf "%6.1f GB (build %5.1f, deps %5.1f) %s\n", $1/1048576, $2/1048576, $3/1048576, $4}'
echo "=====BY REPO"; awk -F'\t' '{n[$5]++; t[$5]+=$1; b[$5]+=$2; p[$5]+=$3} END{for(r in n) printf "%3d wt %6.1f GB (build %5.1f, deps %5.1f) %s\n", n[r], t[r]/1048576, b[r]/1048576, p[r]/1048576, r}' $S/wt.tsv | sort -k3 -rn | head -15

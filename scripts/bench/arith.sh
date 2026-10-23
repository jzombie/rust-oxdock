n=0
i=0
while [ "$i" -lt 1000000 ]; do
  n=$((n + i))
  i=$((i + 1))
done
echo "$n"

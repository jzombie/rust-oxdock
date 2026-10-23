work() {
  __w="  $1  "
  __w="${__w#"${__w%%[![:space:]]*}"}"
  __w="${__w%"${__w##*[![:space:]]}"}"
}
last=""
i=0
while [ "$i" -lt 20000 ]; do
  work "$i"
  last=$__w
  i=$((i + 1))
done
echo "$last"

# rgtop fallback agent for hosts without python3 (see agent.py): same protocol,
# but each sample runs nvidia-smi (~0.2 s). dash-compatible. Every non-GPU line
# is tagged @@x so shell-rc noise can't be mistaken for data.
export LC_ALL=C
n=$(nproc 2>/dev/null || getconf _NPROCESSORS_ONLN 2>/dev/null)
me=$(id -un 2>/dev/null)
command -v nvidia-smi >/dev/null 2>&1 || nosmi=1
while read -r req; do
  [ "$req" = k ] && continue # keepalive from rgtop
  echo @@B
  echo "@@S $(head -n1 /proc/stat 2>/dev/null)"
  if [ -z "$nosmi" ]; then
    nvidia-smi --query-gpu=index,uuid,name,utilization.gpu,memory.used,memory.total,temperature.gpu,power.draw,power.limit --format=csv,noheader,nounits 2>&1
    echo "@@G $?"
  else
    echo "@@G 127"
  fi
  echo "@@S $(head -n1 /proc/stat 2>/dev/null)"
  echo "@@L $(cat /proc/loadavg 2>/dev/null)"
  echo "@@M $(awk '/^MemTotal:/{t=$2} /^MemAvailable:/{a=$2} END{print t+0, a+0}' /proc/meminfo 2>/dev/null)"
  echo "@@N $n"
  echo "@@U $me"
  if [ "$req" != s ] && [ -z "$nosmi" ]; then
    echo @@X
    apps=$(nvidia-smi --query-compute-apps=gpu_uuid,pid,used_memory --format=csv,noheader,nounits 2>/dev/null)
    if [ -n "$apps" ]; then
      printf '%s\n' "$apps" | sed 's/^/@@A /'
      pids=$(printf '%s\n' "$apps" | awk -F', *' '$2 ~ /^[0-9]+$/ {print $2}' | sort -u | paste -sd, -)
      [ -n "$pids" ] && ps -o pid= -o user:32= -o etimes= -o args= -p "$pids" 2>/dev/null | cut -c1-300 | sed 's/^/@@P /'
    fi
  fi
  echo @@E
done

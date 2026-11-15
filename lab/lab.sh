#!/usr/bin/env bash
# The legcon lab: FOSS servers in containers, for integration and GUI tests (lab/README.md).
# Usage: lab/lab.sh build|up|down|status|logs|machines [image]
# Needs only docker (no compose). On Windows run it inside WSL: wsl -- bash lab/lab.sh up
set -euo pipefail
cd "$(dirname "$0")"
IMAGE="${LAB_IMAGE:-legcon-lab/unix90s:dev}"
NAME="${LAB_NAME:-legcon-lab-unix90s}"

# host port -> container port. High host ports keep clear of the host's own SMB, SSH and FTP.
PORTS=(
  10021:21 10121:2121 30000-30009:30000-30009
  10023:23 10512:512 10513:513 10514:514
  10139:139 10445:445
  10022:22 10222:2222 10200:2200
  10111:111 10111:111/udp 12049:12049 12049:12049/udp 12048:12048 12048:12048/udp
  12149:12149 12149:12149/udp 12148:12148 12148:12148/udp
  10548:548 10080:80 10070:70
)

# name kind host-port: what `status` checks and `machines` writes.
SERVICES=(
  "vsftpd ftp 10021" "proftpd ftp 10121" "telnet telnet 10023" "rexec rexec 10512"
  "rlogin rlogin 10513" "rsh rsh 10514" "samba smb 10139" "openssh ssh 10022"
  "dropbear ssh 10222" "ssh1 ssh 10200" "nfs nfs 10111" "nfs2 nfs 10111" "netatalk afp 10548"
  "apache http 10080" "gophernicus gopher 10070"
)
# Checked by `status` only: the NFSv2 server itself (lab-nfs2 reaches it through the portmapper).
EXTRA=("nfs2-nfsd nfs 12149")

up() {
  local args=()
  for p in "${PORTS[@]}"; do args+=(-p "$p"); done
  docker rm -f "$NAME" >/dev/null 2>&1 || true
  docker run -d --name "$NAME" "${args[@]}" \
    -e LAB_HOST="${LAB_HOST:-127.0.0.1}" -e LAB_GOPHER_PORT=10070 "$IMAGE" >/dev/null
  for _ in $(seq 1 60); do
    docker logs "$NAME" 2>&1 | grep -q 'lab: ready' && break
    sleep 1
  done
  status
}

status() {
  local bad=0
  for s in "${SERVICES[@]}" "${EXTRA[@]}"; do
    set -- $s
    if timeout 3 bash -c "exec 3<>/dev/tcp/127.0.0.1/$3" 2>/dev/null; then
      printf '%-12s %-7s %s  up\n' "$1" "$2" "$3"
    else
      printf '%-12s %-7s %s  DOWN\n' "$1" "$2" "$3"; bad=1
    fi
  done
  return $bad
}

# A legcon machines.json with one machine per lab service (user lab, password lab).
machines() {
  local host="${LAB_HOST:-127.0.0.1}" first=1
  echo '{"machines": ['
  for s in "${SERVICES[@]}"; do
    set -- $s
    [ $first = 1 ] || echo ','
    first=0
    printf '  {"id": "lab-%s", "name": "lab-%s", "address": "%s", "services": [{"kind": "%s", "port": %s}], "user": "lab", "password": "b64:bGFi", "check_host_key": false' \
      "$1" "$1" "$host" "$2" "$3"
    [ "$2" = nfs ] && printf ', "uid": 1000, "gid": 1000'
    printf '}'
  done
  printf '\n]}\n'
}

case "${1:-}" in
  build) docker build -t "$IMAGE" containers/unix90s ;;
  up) up ;;
  down) docker rm -f "$NAME" >/dev/null && echo "stopped $NAME" ;;
  status) status ;;
  logs) docker logs "$NAME" ;;
  machines) machines ;;
  *) echo "usage: $0 build|up|down|status|logs|machines" >&2; exit 2 ;;
esac

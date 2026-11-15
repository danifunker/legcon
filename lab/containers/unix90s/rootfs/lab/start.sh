#!/bin/bash
# Start every lab service and stay in the foreground; the container stops when this does.
# LAB_HOST / LAB_GOPHER_PORT: how clients reach the container (gopher menus name it).
set -u
HOST="${LAB_HOST:-127.0.0.1}"
GOPHER_PORT="${LAB_GOPHER_PORT:-10070}"
log() { echo "lab: $*"; }

# Host keys, once per container.
[ -f /etc/ssh/ssh_host_rsa_key ] || ssh-keygen -A >/dev/null
[ -f /etc/dropbear/dropbear_rsa_host_key ] || dropbearkey -t rsa -s 2048 -f /etc/dropbear/dropbear_rsa_host_key >/dev/null 2>&1
[ -f /opt/ssh1/etc/ssh_host_key ] || /opt/ssh1/bin/ssh-keygen -t rsa1 -b 1024 -N '' -f /opt/ssh1/etc/ssh_host_key >/dev/null

# rexecd and rshd refuse clients whose address has no name; name every address on our /24.
net=$(ip -4 -o addr show eth0 | awk '{print $4}' | cut -d. -f1-3)
if [ -n "$net" ]; then
  for i in $(seq 1 254); do echo "$net.$i lab-client-$i"; done >> /etc/hosts
fi

log "portmapper and NFS (unfs3)"
rpcbind -w
/opt/unfs3/sbin/unfsd -e /etc/exports.unfs3 -n 12049 -m 12048
log "NFS v2 (nfs-user-server): nfsd 12149, mountd v1 12148"
# -r: overlayfs has device major 0, which the server would take for an NFS mount and refuse.
/opt/nfs2/sbin/rpc.mountd -r -f /etc/exports.nfs2 -P 12148
/opt/nfs2/sbin/rpc.nfsd -r -f /etc/exports.nfs2 -P 12149

log "FTP: vsftpd on 21, ProFTPD on 2121"
mkdir -p /var/run/vsftpd/empty
vsftpd /etc/vsftpd.lab.conf &
/opt/proftpd/sbin/proftpd -n -c /etc/proftpd/lab.conf &

log "Telnet, rexec, rlogin, rsh (inetd), gopher"
sed -e "s/@HOST@/$HOST/" -e "s/@GOPHER_PORT@/$GOPHER_PORT/" /etc/inetd.lab.conf.in > /etc/inetd.lab.conf
/usr/sbin/inetd /etc/inetd.lab.conf

log "SMB (Samba, SMB1 dialects allowed)"
smbd --foreground --no-process-group &

log "SSH: OpenSSH on 22, Dropbear on 2222, SSH-1 (OpenSSH 7.3) on 2200"
/usr/sbin/sshd -f /etc/ssh/sshd_config.lab
dropbear -R -E -p 2222
/opt/ssh1/sbin/sshd -f /opt/ssh1/etc/sshd_config.lab

log "AFP (Netatalk)"
/opt/netatalk/sbin/netatalk -F /etc/afp.lab.conf

log "HTTP (Apache)"
apache2ctl start

log "ready"
# Stay up even if one service stops, so `lab.sh status` can show which one.
while true; do sleep 3600; done

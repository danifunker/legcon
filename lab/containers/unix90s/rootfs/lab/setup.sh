#!/bin/sh
# Build-time setup: the test user, the fixture tree every protocol serves, and per-server files.
set -eu

# Files copied from a Windows checkout arrive world-writable; servers refuse those.
chmod 755 /lab/*.sh
chmod 644 /etc/vsftpd.lab.conf /etc/proftpd/lab.conf /etc/inetd.lab.conf.in /etc/samba/smb.conf     /etc/ssh/sshd_config.lab /opt/ssh1/etc/sshd_config.lab /etc/exports.unfs3 /etc/exports.nfs2 /etc/afp.lab.conf     /etc/apache2/sites-available/lab.conf

# The test login (lab-only, documented in lab/README.md).
useradd -m -u 1000 -s /bin/bash lab
echo 'lab:lab' | chpasswd
printf 'lab\nlab\n' | smbpasswd -a -s lab

# Fixtures: the same tree in the lab user's home, over every protocol.
H=/home/lab
mkdir -p "$H/notes/sub" "$H/folder with spaces" "$H/deep/a/b/c"
echo 'legcon lab: hello from unix90s' > "$H/readme.txt"
echo 'a note' > "$H/notes/note1.txt"
echo 'deeper' > "$H/notes/sub/note2.txt"
echo 'spaces in the path' > "$H/folder with spaces/file one.txt"
echo 'bottom of the tree' > "$H/deep/a/b/c/leaf.txt"
# A Latin-1 name (cafe with e-acute as byte 0xE9) and names Windows cannot hold.
printf 'latin-1 name\n' > "$H/$(printf 'caf\351.txt')"
echo 'question mark' > "$H/Q1?Q2.txt"
echo 'reserved name' > "$H/CON.txt"
head -c 1048576 /dev/urandom > "$H/big.bin"
echo 'cannot be changed' > "$H/readonly.txt" && chmod 444 "$H/readonly.txt"
ln -s notes "$H/link-to-notes"
mkdir -p "$H/upload"
chown -R lab:lab "$H"

# Anonymous FTP and the web / gopher roots get their own copies.
mkdir -p /srv/ftp/pub /srv/www /srv/gopher
echo 'anonymous ftp: read only' > /srv/ftp/pub/README
cp -a "$H/notes" "$H/readme.txt" "$H/folder with spaces" /srv/ftp/pub/
cp -a "$H/notes" "$H/readme.txt" "$H/folder with spaces" "$H/big.bin" /srv/www/
cp -a "$H/notes" "$H/readme.txt" /srv/gopher/
chown -R root:root /srv/ftp && chmod -R a-w /srv/ftp/pub

# rsh / rlogin trust everyone for the lab user (lab only: never do this on a real machine).
echo '+ +' > "$H/.rhosts" && chown lab:lab "$H/.rhosts" && chmod 600 "$H/.rhosts"

# Apache: folder index pages, no default site.
a2dissite 000-default >/dev/null
a2enmod autoindex >/dev/null
a2ensite lab >/dev/null

# SSH host keys are made at first start (start.sh), so every container has its own.
mkdir -p /var/empty /run/sshd /etc/dropbear /run/rpcbind

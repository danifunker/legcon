# The legcon lab

Real FOSS servers in a container, so legcon is tested against the software it will meet, not only
against the pretend servers in each crate's tests. Everything here is free software and runs on
an ordinary x86-64 machine with Docker.

## unix90s: one container, every protocol

`containers/unix90s` is a Debian 12 image that answers like a 1990s Unix server on every protocol
legcon speaks. The servers that Debian no longer packages are built from source in the same
Dockerfile.

| Service | Server | Port on the host |
|---|---|---|
| FTP | vsftpd 3.0 (anonymous read-only, `lab` read-write) | 10021 (+ passive 30000-30004) |
| FTP | ProFTPD 1.3.8c, built from source | 10121 (+ passive 30005-30009) |
| Telnet | inetutils telnetd | 10023 |
| rexec, rlogin, rsh | netkit rsh-server (`.rhosts` trusts everyone: lab only) | 10512, 10513, 10514 |
| SMB | Samba 4.17 with SMB1 (NT1) and LM / NTLMv1 logins allowed | 10139 (NetBIOS session), 10445 |
| SSH-2 | OpenSSH 9.2 with the old algorithms added back | 10022 |
| SSH-2 | Dropbear 2022.83 | 10222 |
| **SSH-1** | OpenSSH 7.3p1 built with SSH-1 against OpenSSL 1.0.2u, `Protocol 1` only | 10200 |
| NFS v3 | unfs3 0.11 (portmapper on 10111; NFS 12049, mountd v3 12048) | 10111 |
| NFS v2 | nfs-user-server 2.2beta47, the old Linux user-space server, built against libtirpc (NFS 12149, mountd v1 12148) | 10111 |
| AFP over TCP | Netatalk 4.6 (AFP 2.x and 3.x; guest and cleartext logins) | 10548 |
| HTTP | Apache 2.4, folder index pages | 10080 |
| Gopher | Gophernicus 3.1 | 10070 |

The login is user `lab`, password `lab`, on every service. These are lab-only credentials, kept in
this repository on purpose; never reuse them. The lab user's home holds the same fixture tree for
every protocol: `readme.txt`, `notes/`, `folder with spaces/`, a deep path, a 1 MB file, a
read-only file, a symlink, a Latin-1 name (`caf\xe9.txt`), and names Windows cannot hold
(`Q1?Q2.txt`, `CON.txt`). Uploads go to `upload/`.

## Running it

`lab.sh` needs only `docker` (no compose). On Windows, run it inside WSL 2; the published ports
reach Windows on `localhost`.

```sh
lab/lab.sh build      # build legcon-lab/unix90s:dev
lab/lab.sh up         # start it and check every service answers
lab/lab.sh status
lab/lab.sh machines   # a legcon machines.json, one machine per service
lab/lab.sh down
```

Then, from the repository root:

```sh
cargo test --test lab_unix90s -- --ignored                 # protocol round trips, 18 tests
python regression-tests/run_flows.py --lab lab.toml        # the same through the Win32 GUI
```

`tests/lab_unix90s.rs` gives every writable file service the same round trip (the fixtures are
listed; a folder is made; a file goes up, comes back byte for byte, is renamed and deleted; then a
clean log-off) and runs commands over Telnet, rexec, SSH and SSH-1. `regression-tests/flows/lab.toml`
does the same through the window. `LEGCON_LAB_HOST` points the tests at another host.

## Known limits

- **rsh and rlogin** check that the client connects from a port below 1024. Docker's published
  ports (and WSL's forwarding to Windows) rewrite the source port, so those tests run only when
  the client reaches the container directly (`LEGCON_LAB_RSH=1`), as in a pipeline job that runs
  next to it.
- **NFS**: both servers register with the one portmapper, so unfs3 is built without MOUNT v1
  (legcon's NFSv2 client mounts with v1). A client that asks for v3 gets unfs3, v2 gets
  nfs-user-server; both export the same folders. From Windows through WSL only TCP gets through
  (WSL forwards TCP to `localhost`, not UDP), so there NFS is tested over TCP; on Linux both work.
- **Windows FTP listings** (IIS, NT 3.5's FTP server) have no FOSS equivalent; they are covered by
  recorded listings in `lc-ftp`'s tests and by real NT machines.

## Bugs it found (2026-11-14)

- AFP: an empty folder could not be opened (the first FPEnumerate of an empty folder answers
  "object not found").
- NFS over TCP: back-to-back connections failed with "address in use" (the reserved source port
  was still in TIME_WAIT).
- rexec and rsh: command output was lost behind NAT-like relays because legcon half-closed the
  connection after sending the command.
- Gopher: names included Gophernicus's date and size columns.
- SSH: closing a Dropbear terminal could hang for good (Dropbear does not always answer the
  channel close); legcon now waits two seconds, then logs off.

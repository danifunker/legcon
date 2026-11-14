# legcon lab: one container that answers like a 1990s Unix server on every protocol legcon speaks.
# FOSS only. See lab/README.md. Build: lab/lab.sh build. Test login: lab / lab.

# --- SSH-1: OpenSSH 7.3p1 (the last with SSH-1) against OpenSSL 1.0.2u, built static ---
FROM debian:bookworm-slim AS ssh1
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential ca-certificates curl perl zlib1g-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /src
RUN curl -fsSLO https://www.openssl.org/source/old/1.0.2/openssl-1.0.2u.tar.gz \
 && curl -fsSLO https://cdn.openbsd.org/pub/OpenBSD/OpenSSH/portable/openssh-7.3p1.tar.gz \
 && tar xzf openssl-1.0.2u.tar.gz && tar xzf openssh-7.3p1.tar.gz
RUN cd openssl-1.0.2u && ./config no-shared --prefix=/opt/ssl1 && make -j"$(nproc)" && make install_sw
RUN cd openssh-7.3p1 && ./configure --prefix=/opt/ssh1 --sysconfdir=/opt/ssh1/etc \
        --with-ssl-dir=/opt/ssl1 --with-ssh1 --without-pam --with-privsep-path=/var/empty \
 && make -j"$(nproc)" && make install-nokeys

# --- NFS v3 in user space: unfs3 ---
FROM debian:bookworm-slim AS unfs3
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential ca-certificates curl flex bison pkg-config libtirpc-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /src
RUN curl -fsSL https://github.com/unfs3/unfs3/releases/download/unfs3-0.11.0/unfs3-0.11.0.tar.gz | tar xz
RUN cd unfs3-0.11.0 && ./configure --prefix=/opt/unfs3 && make && make install

# --- AFP over TCP: Netatalk (keeps the classic-Mac AFP 2.x support of the 2.x line) ---
FROM debian:bookworm-slim AS netatalk
RUN apt-get update && apt-get install -y --no-install-recommends \
        build-essential ca-certificates curl xz-utils meson ninja-build pkg-config cmark-gfm \
        libgcrypt20-dev libevent-dev libdb-dev libiniparser-dev libacl1-dev libattr1-dev \
        libtalloc-dev libsqlite3-dev libwolfssl-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /src
RUN curl -fsSL https://github.com/Netatalk/netatalk/releases/download/netatalk-4-6-0/netatalk-4.6.0.tar.xz | tar xJ
# Debian 12's glibc has no strlcpy (2.38 added it); libbsd supplies it.
RUN apt-get update && apt-get install -y --no-install-recommends libbsd-dev && rm -rf /var/lib/apt/lists/*
RUN cd netatalk-4.6.0 && CFLAGS="-include bsd/string.h" LDFLAGS="-lbsd" \
        meson setup build --prefix=/opt/netatalk -Dwith-tests=false \
        -Dwith-spotlight=false -Dwith-zeroconf=false -Dwith-pam=false -Dwith-cups=false \
 && meson compile -C build && meson install -C build

# --- ProFTPD from source (Debian's package conflicts with vsftpd's) ---
FROM debian:bookworm-slim AS proftpd
RUN apt-get update && apt-get install -y --no-install-recommends         build-essential ca-certificates curl && rm -rf /var/lib/apt/lists/*
WORKDIR /src
RUN curl -fsSL https://github.com/proftpd/proftpd/archive/refs/tags/v1.3.8c.tar.gz | tar xz
RUN cd proftpd-1.3.8c && ./configure --prefix=/opt/proftpd --disable-ipv6 && make -j"$(nproc)" && make install

# --- the server ---
FROM debian:bookworm-slim
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get install -y --no-install-recommends \
        vsftpd openbsd-inetd inetutils-telnetd rsh-server samba \
        openssh-server dropbear-bin rpcbind libtirpc3 apache2 gophernicus \
        libgcrypt20 libevent-2.1-7 libdb5.3 libiniparser1 libacl1 libattr1 libtalloc2 libsqlite3-0 \
        libwolfssl35 libbsd0 procps iproute2 netbase \
 && rm -rf /var/lib/apt/lists/*
COPY --from=ssh1 /opt/ssh1 /opt/ssh1
COPY --from=unfs3 /opt/unfs3 /opt/unfs3
COPY --from=proftpd /opt/proftpd /opt/proftpd
COPY --from=netatalk /opt/netatalk /opt/netatalk
COPY rootfs/ /
RUN sh /lab/setup.sh
# FTP 21 / 2121 (+ passive 30000-30009), Telnet 23, rexec 512, rlogin 513, rsh 514, SMB 139 / 445,
# SSH 22, Dropbear 2222, SSH-1 2200, portmapper 111, NFS 12049, mountd 12048, AFP 548, HTTP 80, Gopher 70.
EXPOSE 21 2121 23 512 513 514 139 445 22 2222 2200 111 12049 12048 548 80 70
CMD ["/lab/start.sh"]

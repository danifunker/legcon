/* legcon lab: libtirpc's svc_getcaller is a sockaddr_in6; the server's own sockets are IPv4. */
#undef svc_getcaller
#define svc_getcaller(x) ((struct sockaddr_in *) &(x)->xp_raddr)

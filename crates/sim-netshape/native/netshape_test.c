// Host tests and microbenchmarks for netshape.c's portable core: profile parsing, the published
// profile, rule matching and its cache, per-fd state and the I/O gates, on real host sockets.
// tests/native.rs builds and runs it with the host C compiler:
//
//   cc -std=gnu11 -O2 -pthread -o netshape_test native/netshape_test.c
//   ./netshape_test                          # tests; exit status 0 when all pass
//   ./netshape_test bench                    # ns per call of the hot paths
//   ./netshape_test effective <conf> <host>  # the effective profile per host (compared with Rust's)
#define NETSHAPE_HOST_TEST 1
static unsigned test_roll = 99;               // what NETSHAPE_RANDOM(100) returns
#define NETSHAPE_RANDOM(n) (test_roll % (n))
static _Atomic unsigned long test_matches, test_sockopts;
#define NETSHAPE_COUNT(what) atomic_fetch_add_explicit(&test_##what, 1, memory_order_relaxed)
#include "netshape.c"

#include <netinet/tcp.h>

static int failures;
#define CHECK(cond)                                                                                                    \
    do {                                                                                                               \
        if (!(cond)) {                                                                                                 \
            fprintf(stderr, "%s:%d: CHECK(%s) failed\n", __FILE__, __LINE__, #cond);                                   \
            failures++;                                                                                                \
        }                                                                                                              \
    } while (0)

static char conf_path[256];

/// Writes the profile file and makes the next reload read it (a new mtime, the check due now).
static void set_conf(const char *text) {
    static long stamp = 1000;
    FILE *f = fopen(conf_path, "w");
    fputs(text, f);
    fclose(f);
    struct timespec times[2] = {{stamp, 0}, {stamp, 0}};
    stamp++;
    utimensat(AT_FDCWD, conf_path, times, 0);
    atomic_store(&g_next_check, 0);
    reload();
}

static struct sockaddr_storage addr(const char *text, int port) {
    struct sockaddr_storage ss;
    memset(&ss, 0, sizeof ss);
    struct sockaddr_in *a4 = (struct sockaddr_in *)&ss;
    struct sockaddr_in6 *a6 = (struct sockaddr_in6 *)&ss;
    if (inet_pton(AF_INET, text, &a4->sin_addr) == 1) {
        a4->sin_family = AF_INET;
        a4->sin_port = htons((uint16_t)port);
    } else if (inet_pton(AF_INET6, text, &a6->sin6_addr) == 1) {
        a6->sin6_family = AF_INET6;
        a6->sin6_port = htons((uint16_t)port);
    }
    return ss;
}

static int match(const char *text) {
    struct sockaddr_storage ss = addr(text, 443);
    return match_rule((struct sockaddr *)&ss);
}

static double elapsed_ms(int64_t since) { return (double)(now_ns() - since) / 1e6; }

// ---- tests ----

static void test_parse(void) {
    parser_t ps;
    parse_begin(&ps);
    const char *text = "offline=0\nlatency_ms = 250\ndown_kbps=800\nup_kbps=80\nloss_pct=5\nport_base=41000\nport_count=10\n"
                       "rule 10.0.0.0/8 latency_ms=7\n"
                       "rule 2001:db8::/32 offline=1\n"
                       "rule example.com down_kbps=8 up_kbps=16 loss_pct=1\n"
                       "rule * offline=1\n"
                       "rule 192.168.1.5\n"
                       "rule 10.0.0.0/x\n"
                       "rule 10.0.0.0/40\n"
                       "rule\n"
                       "garbage\n";
    parse_feed(&ps, text, strlen(text));
    parse_end(&ps);
    CHECK(ps.base.latency_ms == 250 && ps.base.down_bps == 100000 && ps.base.up_bps == 10000 && ps.base.loss_pct == 5);
    CHECK(ps.base.port_base == 41000 && ps.base.port_count == 10 && !ps.base.offline);
    CHECK(ps.nrules == 7);
    CHECK(ps.rules[0].family == AF_INET && ps.rules[0].bits == 8 && ps.rules[0].set_latency && ps.rules[0].latency_ms == 7);
    CHECK(ps.rules[1].family == AF_INET6 && ps.rules[1].bits == 32 && ps.rules[1].set_offline && ps.rules[1].offline);
    CHECK(ps.rules[2].family == 0 && ps.rules[2].down_bps == 1000 && ps.rules[2].up_bps == 2000 && ps.rules[2].set_loss);
    CHECK(ps.rules[3].any && !ps.rules[2].any);
    CHECK(ps.rules[4].family == AF_INET && ps.rules[4].bits == 32);
    CHECK(ps.rules[5].bits == 0);  // atoi("x"), as the per-call parser read it
    CHECK(ps.rules[6].bits == 32); // out of range: the whole address

    // At most MAX_RULES rules; lines are cut at 511 bytes (fgets with a 512-byte buffer).
    parse_begin(&ps);
    for (int i = 0; i < MAX_RULES + 5; i++) {
        char line[64];
        int n = snprintf(line, sizeof line, "rule host%d.test latency_ms=%d\n", i, i);
        parse_feed(&ps, line, (size_t)n);
    }
    CHECK(ps.nrules == MAX_RULES && ps.rules[MAX_RULES - 1].latency_ms == MAX_RULES - 1);
    parse_begin(&ps);
    char longline[600];
    memset(longline, 'x', 511);
    strcpy(longline + 511, "latency_ms=9\n");
    for (size_t i = 0; i < strlen(longline); i++) parse_feed(&ps, longline + i, 1); // fed byte by byte
    parse_feed(&ps, "up_kbps=8", 9);                                               // no final newline
    parse_end(&ps);
    CHECK(ps.base.latency_ms == 9 && ps.base.up_bps == 1000);
}

static void test_reload_and_effective(void) {
    set_conf("latency_ms=100\ndown_kbps=800\nrule 10.0.0.0/8 latency_ms=5 offline=1\nrule slow.test up_kbps=8\n");
    profile_t p = effective(0);
    CHECK(p.latency_ms == 100 && p.down_bps == 100000 && !p.offline);
    p = effective(1);
    CHECK(p.latency_ms == 5 && p.offline && p.down_bps == 100000);
    p = effective(2);
    CHECK(p.latency_ms == 100 && p.up_bps == 1000);
    CHECK(effective(3).latency_ms == 100);  // past the rules: the base profile
    CHECK(effective(-1).latency_ms == 100);

    // An unchanged file isn't re-read; a changed one is, once the check is due.
    unsigned seq = atomic_load(&g_pub_seq);
    atomic_store(&g_next_check, 0);
    reload();
    CHECK(atomic_load(&g_pub_seq) == seq);
    set_conf("latency_ms=7\n");
    CHECK(effective(0).latency_ms == 7 && atomic_load(&g_nrules_hint) == 0);
    CHECK(effective(1).latency_ms == 7);

    // Not due yet: the published profile stands.
    FILE *f = fopen(conf_path, "w");
    fputs("latency_ms=8\n", f);
    fclose(f);
    atomic_store(&g_next_check, now_ns() + CHECK_NS);
    CHECK(effective(0).latency_ms == 7);

    // No file: the base profile is cleared and the rules stay (as before).
    set_conf("latency_ms=9\nrule * latency_ms=3\n");
    unlink(conf_path);
    atomic_store(&g_next_check, 0);
    CHECK(effective(0).latency_ms == 0 && effective(1).latency_ms == 3);
    set_conf("");
}

static void test_match(void) {
    set_conf("rule 10.0.0.0/8 latency_ms=1\n"
             "rule 2001:db8::/32 latency_ms=2\n"
             "rule 192.168.1.5 latency_ms=3\n"
             "rule 172.16.0.0/12 latency_ms=4\n"
             "rule Example.COM latency_ms=5\n"
             "rule 0.0.0.0/0 latency_ms=6\n");
    CHECK(match("10.2.3.4") == 1);
    CHECK(match("2001:db8:1::5") == 2);
    CHECK(match("2001:db9::5") == 0);
    CHECK(match("192.168.1.5") == 3);
    CHECK(match("192.168.1.6") == 6);
    CHECK(match("172.31.255.255") == 4 && match("172.32.0.0") == 6);
    CHECK(match("::ffff:10.1.1.1") == 0); // a mapped address isn't an IPv4 CIDR's, as before
    CHECK(match_rule(NULL) == 0);

    // Hostname rules match addresses through learned names, case-insensitively, by suffix.
    struct sockaddr_storage v6 = addr("2001:db9::7", 443);
    CHECK(match_rule((struct sockaddr *)&v6) == 0);
    learn("API.example.com.", (struct sockaddr *)&v6);
    CHECK(match_rule((struct sockaddr *)&v6) == 5);
    learn("badexample.com", (struct sockaddr *)&v6);
    CHECK(match_rule((struct sockaddr *)&v6) == 0);
    CHECK(match_rule_name("www.example.com.") == 5 && match_rule_name("example.org") == 0 && match_rule_name(NULL) == 0);

    set_conf("rule * latency_ms=1\n");
    CHECK(match("8.8.8.8") == 1 && match("2001:db8::1") == 1 && match_rule_name("x.y") == 1);
    set_conf("");
    unsigned long before = atomic_load(&test_matches);
    CHECK(match("10.0.0.1") == 0 && match_rule_name("example.com") == 0);
    CHECK(atomic_load(&test_matches) == before); // no rules: nothing scanned
}

static int udp_socket(int family) { return my_socket(family, SOCK_DGRAM, 0); }

/// Whether this host can open an IPv6 socket and bind it to ::1. A container without IPv6 can't: the
/// IPv6 cases are skipped there, with the reason printed, and the IPv4 cases still run.
static int ipv6_usable(const char *what) {
    int fd = socket(AF_INET6, SOCK_DGRAM, 0);
    int err = fd < 0 ? errno : 0;
    if (fd >= 0) {
        struct sockaddr_in6 lo = {0};
        lo.sin6_family = AF_INET6;
        lo.sin6_addr = in6addr_loopback;
        if (bind(fd, (struct sockaddr *)&lo, sizeof lo) != 0) err = errno;
        close(fd);
    }
    if (err == 0) return 1;
    if (err != EAFNOSUPPORT && err != EADDRNOTAVAIL && err != EPROTONOSUPPORT) {
        errno = err;
        CHECK(!"IPv6 probe failed for a reason other than missing IPv6 support");
        return 0;
    }
    fprintf(stderr, "SKIPPED: %s: IPv6 is not available on this host (%s)\n", what, strerror(err));
    return 0;
}

static void test_fd_state(void) {
    set_conf("loss_pct=0\nrule 198.51.100.0/24 latency_ms=40\n");
    struct sockaddr_storage remote = addr("198.51.100.7", 53), other = addr("203.0.113.9", 53), local = addr("127.0.0.1", 53);

    int u = udp_socket(AF_INET);
    CHECK(u >= 0 && fd_get(u) == 0);
    CHECK(admit(u, (struct sockaddr *)&remote) == 0);
    uint32_t st = fd_get(u);
    CHECK((st & (FD_SHAPED | FD_TYPED | FD_DGRAM)) == (FD_SHAPED | FD_TYPED | FD_DGRAM) && fd_rule(st) == 1);
    CHECK(admit(u, (struct sockaddr *)&other) == 0 && fd_rule(fd_get(u)) == 0);
    fd_update(u, 0, FD_TSTAMP | FD_DEBT);
    CHECK(admit(u, (struct sockaddr *)&local) == 0 && fd_get(u) == FD_TSTAMP); // loopback: unshaped (timestamps stay on)
    CHECK(admit(u, (struct sockaddr *)&remote) == 0);
    CHECK(my_close(u) == 0 && fd_get(u) == 0); // close clears everything, SO_TIMESTAMP's flag included

    int t = my_socket(AF_INET, SOCK_STREAM, 0);
    set_conf("loss_pct=0\nlatency_ms=0\n");
    CHECK(admit(t, (struct sockaddr *)&remote) == 0);
    CHECK((fd_get(t) & (FD_SHAPED | FD_TYPED | FD_DGRAM)) == (FD_SHAPED | FD_TYPED));

    // A number closed where we can't see it comes back clear from socket(), accept(), dup and dup2.
    atomic_store(&g_fd[t], FD_SHAPED | FD_DGRAM | FD_TSTAMP);
    close(t);
    int again = my_socket(AF_INET, SOCK_STREAM, 0);
    CHECK(again == t && fd_get(again) == 0);
    int pipefd[2];
    CHECK(pipe(pipefd) == 0);
    atomic_store(&g_fd[pipefd[1]], FD_SHAPED);
    CHECK(my_dup2(pipefd[0], pipefd[1]) == pipefd[1] && fd_get(pipefd[1]) == 0);
    atomic_store(&g_fd[pipefd[1]], FD_SHAPED);
    CHECK(my_dup2(-1, pipefd[1]) == -1 && fd_get(pipefd[1]) == FD_SHAPED); // a failed dup2 closes nothing
    CHECK(my_dup2(pipefd[1], pipefd[1]) == pipefd[1] && fd_get(pipefd[1]) == FD_SHAPED);
    int d = dup(pipefd[0]);
    close(d);
    atomic_store(&g_fd[d], FD_SHAPED);
    CHECK(my_dup(pipefd[0]) == d && fd_get(d) == 0);
    my_close(d);
    my_close(pipefd[0]);
    my_close(pipefd[1]);
    my_close(again);

    // Offline refuses remote connects only.
    set_conf("offline=1\n");
    int o = my_socket(AF_INET, SOCK_STREAM, 0);
    errno = 0;
    CHECK(admit(o, (struct sockaddr *)&remote) == -1 && errno == ENETUNREACH);
    CHECK(admit(o, (struct sockaddr *)&local) == 0);
    my_close(o);
    set_conf("");
}

static void test_datagram_gate_and_cache(void) {
    set_conf("rule 198.51.100.0/24 offline=1\n"
             "rule 203.0.113.0/24 loss_pct=50\n"
             "rule cached.test latency_ms=9\n");
    struct sockaddr_storage off = addr("198.51.100.7", 53), lossy = addr("203.0.113.9", 53), plain = addr("192.0.2.1", 53),
                            v6 = addr("2001:db8::1", 53), local = addr("127.0.0.1", 53);
    int u = udp_socket(AF_INET);
    errno = 0;
    CHECK(send_gate(u, (struct sockaddr *)&off, "x", 1) == -1 && errno == ENETUNREACH);
    test_roll = 10;
    CHECK(send_gate(u, (struct sockaddr *)&lossy, "x", 1) == 1);
    test_roll = 90;
    CHECK(send_gate(u, (struct sockaddr *)&lossy, "x", 1) == 0);
    test_roll = 99;
    CHECK(send_gate(u, (struct sockaddr *)&plain, "x", 1) == 0 && fd_rule(fd_get(u)) == 0);
    // A loopback destination leaves the socket's state alone; a connected (NULL) send uses it.
    CHECK(send_gate(u, (struct sockaddr *)&local, "x", 1) == 0 && (fd_get(u) & FD_SHAPED));

    // One match per destination while rules and names stay the same; one socket-type read per socket.
    unsigned long m0 = atomic_load(&test_matches), s0 = atomic_load(&test_sockopts);
    for (int i = 0; i < 100; i++) CHECK(send_gate(u, (struct sockaddr *)&plain, "x", 1) == 0);
    CHECK(atomic_load(&test_matches) - m0 == 0 && atomic_load(&test_sockopts) == s0);
    CHECK(send_gate(u, (struct sockaddr *)&lossy, "x", 1) == 0);
    CHECK(send_gate(u, (struct sockaddr *)&lossy, "x", 1) == 0 && fd_rule(fd_get(u)) == 2);
    CHECK(atomic_load(&test_matches) - m0 == 1);
    // A name learned for the address can change its rule: the cache misses once.
    learn("a.cached.test", (struct sockaddr *)&lossy);
    CHECK(send_gate(u, (struct sockaddr *)&lossy, "x", 1) == 0 && atomic_load(&test_matches) - m0 == 2 && fd_rule(fd_get(u)) == 2);
    learn("a.cached.test", (struct sockaddr *)&lossy); // the same name again: nothing changes
    CHECK(send_gate(u, (struct sockaddr *)&lossy, "x", 1) == 0 && atomic_load(&test_matches) - m0 == 2);
    learn("a.cached.test", (struct sockaddr *)&plain);
    CHECK(send_gate(u, (struct sockaddr *)&plain, "x", 1) == 0 && fd_rule(fd_get(u)) == 3);
    // New rules: matched again under them.
    set_conf("rule 192.0.2.0/24 latency_ms=1\n");
    CHECK(send_gate(u, (struct sockaddr *)&plain, "x", 1) == 0 && fd_rule(fd_get(u)) == 1);
    // IPv6 destinations are cached by their whole address.
    if (ipv6_usable("IPv6 destination cache cases")) {
        int u6 = udp_socket(AF_INET6);
        set_conf("rule 2001:db8::1 latency_ms=1\n");
        struct sockaddr_storage v6b = addr("2001:db8::2", 53);
        CHECK(send_gate(u6, (struct sockaddr *)&v6, "x", 1) == 0 && fd_rule(fd_get(u6)) == 1);
        CHECK(send_gate(u6, (struct sockaddr *)&v6b, "x", 1) == 0 && fd_rule(fd_get(u6)) == 0);
        CHECK(send_gate(u6, (struct sockaddr *)&v6, "x", 1) == 0 && fd_rule(fd_get(u6)) == 1);
        my_close(u6);
    }
    my_close(u);
    set_conf("");
}

static void test_unshaped_paths(void) {
    set_conf("latency_ms=500\nup_kbps=8\ndown_kbps=8\nrule * latency_ms=500\n");
    int sp[2];
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sp) == 0);
    unsigned long m0 = atomic_load(&test_matches), s0 = atomic_load(&test_sockopts);
    int64_t t0 = now_ns();
    char buf[4096] = {0};
    for (int i = 0; i < 100; i++) {
        CHECK(my_send(sp[0], buf, 1000, 0) == 1000 && my_recv(sp[1], buf, sizeof buf, 0) == 1000);
        CHECK(my_write(sp[1], buf, 10) == 10 && my_read(sp[0], buf, sizeof buf) == 10);
        struct iovec v = {buf, 5};
        CHECK(my_writev(sp[0], &v, 1) == 5 && my_readv(sp[1], &v, 1) == 5);
        struct msghdr m = {0};
        m.msg_iov = &v;
        m.msg_iovlen = 1;
        CHECK(my_sendmsg(sp[0], &m, 0) == 5 && my_recvmsg(sp[1], &m, 0) == 5);
        CHECK(my_sendto(sp[0], buf, 3, 0, NULL, 0) == 3 && my_recvfrom(sp[1], buf, 3, 0, NULL, NULL) == 3);
    }
    // Unshaped: no rule matched, no getsockopt, no state, no shaping delay.
    CHECK(atomic_load(&test_matches) == m0 && atomic_load(&test_sockopts) == s0);
    CHECK(fd_get(sp[0]) == 0 && fd_get(sp[1]) == 0);
    CHECK(elapsed_ms(t0) < 400);
    my_close(sp[0]);
    my_close(sp[1]);
    set_conf("");
}

/// A connected TCP pair over loopback, the client end marked shaped as if its peer were remote.
static void tcp_pair(int *client, int *server) {
    int l = my_socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_storage any = addr("127.0.0.1", 0);
    socklen_t len = sizeof(struct sockaddr_in);
    CHECK(bind(l, (struct sockaddr *)&any, len) == 0 && listen(l, 1) == 0);
    CHECK(getsockname(l, (struct sockaddr *)&any, &len) == 0);
    *client = my_socket(AF_INET, SOCK_STREAM, 0);
    CHECK(my_connect(*client, (struct sockaddr *)&any, len) == 0);
    *server = my_accept(l, NULL, NULL);
    my_close(l);
    int one = 1;
    setsockopt(*client, IPPROTO_TCP, TCP_NODELAY, &one, sizeof one);
    CHECK(fd_get(*client) == 0); // loopback isn't shaped
    atomic_store(&g_fd[*client], FD_SHAPED | FD_TYPED);
}

static void test_bucket(void) {
    // The link is shared: bytes queue behind the ones already in flight (100 KB/s, 50 ms booked).
    _Atomic int64_t free_at = now_ns() + 50000000;
    int64_t t0 = now_ns();
    bucket_wait(&free_at, 1000, 100000);
    CHECK(elapsed_ms(t0) >= 58);
    CHECK(atomic_load(&free_at) - t0 >= 59000000);
}

static void test_tcp_pacing(void) {
    int c, s;
    char buf[16384] = {0};
    tcp_pair(&c, &s);
    // Upload at 800 kbit/s (100 KB/s): 10 KB takes at least 100 ms.
    set_conf("up_kbps=800\n");
    int64_t t0 = now_ns();
    CHECK(my_send(c, buf, 10000, 0) == 10000);
    CHECK(elapsed_ms(t0) >= 95);
    CHECK(recv(s, buf, 10000, MSG_WAITALL) == 10000);
    // Latency: a receive after a send owes one round trip; a second receive doesn't.
    set_conf("latency_ms=60\n");
    CHECK(my_send(c, "ping", 4, 0) == 4 && (fd_get(c) & FD_SENT));
    CHECK(send(s, "pong", 4, 0) == 4 && send(s, "more", 4, 0) == 4);
    t0 = now_ns();
    CHECK(my_recv(c, buf, 4, MSG_WAITALL) == 4);
    CHECK(elapsed_ms(t0) >= 55 && !(fd_get(c) & FD_SENT));
    // The round-trip debt a renamed connection owes is paid once, too.
    fd_update(c, 0, FD_DEBT);
    t0 = now_ns();
    CHECK(my_read(c, buf, 4) == 4 && elapsed_ms(t0) >= 55 && !(fd_get(c) & FD_DEBT));
    // Loss stalls a TCP stream (200 ms plus a round trip) instead of dropping bytes.
    set_conf("loss_pct=50\n");
    test_roll = 10;
    t0 = now_ns();
    CHECK(my_write(c, "x", 1) == 1 && elapsed_ms(t0) >= 195);
    test_roll = 99;
    CHECK(recv(s, buf, 1, 0) == 1);
    my_close(c);
    my_close(s);
    set_conf("");
}

static size_t client_hello(unsigned char *b, const char *host) {
    size_t n = strlen(host), i = 0;
    b[i++] = 0x16, b[i++] = 0x03, b[i++] = 0x01, b[i++] = 0, b[i++] = 0; // record header (length unchecked)
    b[i++] = 0x01, b[i++] = 0, b[i++] = 0, b[i++] = 0;                   // ClientHello
    b[i++] = 0x03, b[i++] = 0x03;
    memset(b + i, 0, 32), i += 32;                                       // random
    b[i++] = 0;                                                          // session id
    b[i++] = 0, b[i++] = 2, b[i++] = 0x13, b[i++] = 0x01;                // cipher suites
    b[i++] = 1, b[i++] = 0;                                              // compression
    size_t ext = 4 + 5 + n;
    b[i++] = (unsigned char)(ext >> 8), b[i++] = (unsigned char)ext;
    b[i++] = 0, b[i++] = 0;                                              // server_name
    b[i++] = (unsigned char)((5 + n) >> 8), b[i++] = (unsigned char)(5 + n);
    b[i++] = (unsigned char)((3 + n) >> 8), b[i++] = (unsigned char)(3 + n);
    b[i++] = 0;
    b[i++] = (unsigned char)(n >> 8), b[i++] = (unsigned char)n;
    memcpy(b + i, host, n);
    return i + n;
}

static void test_inspect_first(void) {
    unsigned char hello[256];
    char name[128];
    size_t n = client_hello(hello, "api.slow.test");
    CHECK(tls_sni(hello, n, name, sizeof name) && !strcmp(name, "api.slow.test"));
    const char *req = "GET / HTTP/1.1\r\nHost: www.example.com:8080\r\n\r\n";
    CHECK(http_host((const unsigned char *)req, strlen(req), name, sizeof name) && !strcmp(name, "www.example.com"));
    CHECK(!http_host((const unsigned char *)"BREW / HTCPCP\r\n", 15, name, sizeof name));

    // The SNI names the connection: its rule changes, and the handshake's round trip is owed.
    set_conf("rule slow.test latency_ms=10\nrule blocked.test offline=1\n");
    int c, s;
    tcp_pair(&c, &s);
    CHECK(my_send(c, hello, n, 0) == (ssize_t)n);
    uint32_t st = fd_get(c);
    CHECK(fd_rule(st) == 1 && (st & FD_DEBT) && (st & FD_SEEN));
    CHECK(my_send(c, hello, n, 0) == (ssize_t)n); // only the first bytes are inspected
    my_close(c);
    my_close(s);
    tcp_pair(&c, &s);
    n = client_hello(hello, "www.blocked.test");
    errno = 0;
    CHECK(my_send(c, hello, n, 0) == -1 && errno == ECONNRESET);
    my_close(c);
    my_close(s);
    set_conf("");
}

static void test_udp_delay(void) {
    // A shaped datagram socket with latency turns SO_TIMESTAMP on once and delays delivery.
    set_conf("latency_ms=50\n");
    int a = udp_socket(AF_INET), b = udp_socket(AF_INET);
    struct sockaddr_storage any = addr("127.0.0.1", 0);
    socklen_t len = sizeof(struct sockaddr_in);
    CHECK(bind(b, (struct sockaddr *)&any, len) == 0 && getsockname(b, (struct sockaddr *)&any, &len) == 0);
    atomic_store(&g_fd[b], FD_SHAPED | FD_TYPED | FD_DGRAM);
    profile_t p;
    CHECK(udp_delayed(b, &p) && p.latency_ms == 50 && (fd_get(b) & FD_TSTAMP));
    CHECK(sendto(a, "x", 1, 0, (struct sockaddr *)&any, len) == 1);
    int64_t t0 = now_ns();
    char buf[8];
    CHECK(my_recv(b, buf, sizeof buf, 0) == 1 && elapsed_ms(t0) >= 45);
    my_close(a);
    my_close(b);
    set_conf("");
}

// ---- concurrency ----

static _Atomic int stop;

static void *publisher(void *arg) {
    (void)arg;
    profile_t a = {1, 1, 1, 1, 1, 1, 1}, b = {0, 2, 2, 2, 2, 2, 2};
    rule_t rule = {.set_latency = 1, .latency_ms = 1};
    for (int i = 0; !atomic_load(&stop); i++) {
        rule.latency_ms = i % 2 ? 1 : 2;
        publish(i % 2 ? &a : &b, &rule, 1);
    }
    return NULL;
}

static void *profile_reader(void *arg) {
    long torn = 0;
    while (!atomic_load(&stop)) {
        for (int rule = 0; rule <= 1; rule++) {
            profile_t p = effective(rule);
            long v = p.latency_ms; // every field of one publish carries the same value
            if (p.down_bps != v || p.up_bps != v || p.loss_pct != v || p.port_base != v || p.port_count != v ||
                p.offline != (v == 1))
                torn++;
        }
    }
    *(long *)arg = torn;
    return NULL;
}

static void test_latch(void) {
    atomic_store(&g_next_check, INT64_MAX); // no file reloads: only the publisher writes
    atomic_store(&stop, 0);
    pthread_t w, r[3];
    long torn[3] = {0};
    pthread_create(&w, NULL, publisher, NULL);
    for (int i = 0; i < 3; i++) pthread_create(&r[i], NULL, profile_reader, &torn[i]);
    struct timespec ts = {0, 300000000};
    nanosleep(&ts, NULL);
    atomic_store(&stop, 1);
    pthread_join(w, NULL);
    for (int i = 0; i < 3; i++) {
        pthread_join(r[i], NULL);
        CHECK(torn[i] == 0);
    }
    set_conf("");
}

static void *dst_hammer(void *arg) {
    long id = (long)arg, wrong = 0;
    for (unsigned i = 0; !atomic_load(&stop); i++) {
        uint32_t key[5] = {AF_INET6, i % 7, (uint32_t)id, i % 7, 0};
        int want = (int)(i % 7 + (unsigned)id * 10); // each key's rule is a function of the key
        if (i % 3 == 0) dst_store(5, key, 1, want);
        int got = dst_lookup(5, key, 1);
        if (got != -1 && got != want) wrong++;
    }
    return (void *)wrong;
}

static void test_dst_concurrency(void) {
    atomic_store(&stop, 0);
    pthread_t t[4];
    for (long i = 0; i < 4; i++) pthread_create(&t[i], NULL, dst_hammer, (void *)i);
    struct timespec ts = {0, 300000000};
    nanosleep(&ts, NULL);
    atomic_store(&stop, 1);
    for (int i = 0; i < 4; i++) {
        void *wrong;
        pthread_join(t[i], &wrong);
        CHECK(wrong == NULL);
    }
}

// ---- benchmarks ----

static double bench_ns(const char *label, long iters, void (*body)(void)) {
    for (long i = 0; i < iters / 10; i++) body(); // warm up
    int64_t t0 = now_ns();
    for (long i = 0; i < iters; i++) body();
    double ns = (double)(now_ns() - t0) / (double)iters;
    printf("%-58s %9.1f ns\n", label, ns);
    return ns;
}

static int b_sp[2], b_tcp, b_udp;
static struct sockaddr_storage b_to;
static char b_buf[64];
static void b_raw_send_recv(void) { send(b_sp[0], b_buf, 64, 0), recv(b_sp[1], b_buf, 64, 0); }
static void b_my_send_recv(void) { my_send(b_sp[0], b_buf, 64, 0), my_recv(b_sp[1], b_buf, 64, 0); }
static void b_raw_write_read(void) {
    if (write(b_sp[0], b_buf, 64) == 64) (void)!read(b_sp[1], b_buf, 64);
}
static void b_my_write_read(void) { my_write(b_sp[0], b_buf, 64), my_read(b_sp[1], b_buf, 64); }
static void b_getsockopt(void) { (void)sock_type(b_sp[0]); }
// The shaper's own work around an unshaped socket's send (what my_send adds besides the syscall).
static void b_unshaped_gate(void) { (void)send_gate(b_sp[0], NULL, b_buf, 64), pace(b_sp[0], 64, 1); }
static void b_effective(void) { (void)effective(1); }
static void b_gate_shaped_tcp(void) { (void)send_gate(b_tcp, NULL, b_buf, 64), pace(b_tcp, 64, 1), pace(b_tcp, 64, 0); }
static void b_gate_udp_to(void) { (void)send_gate(b_udp, (struct sockaddr *)&b_to, b_buf, 64); }
static void b_match(void) { (void)match_rule((struct sockaddr *)&b_to); }

static void bench(void) {
    printf("netshape hot paths (64-byte messages; host %s)\n", __VERSION__);
    set_conf("");
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, b_sp) == 0);
    long n = 400000;
    double raw = bench_ns("unshaped send+recv: raw syscalls", n, b_raw_send_recv);
    double mine = bench_ns("unshaped send+recv: through the shaper", n, b_my_send_recv);
    printf("%-58s %9.1f ns\n", "  shaper overhead per send+recv pair", mine - raw);
    raw = bench_ns("unshaped write+read: raw syscalls", n, b_raw_write_read);
    mine = bench_ns("unshaped write+read: through the shaper", n, b_my_write_read);
    printf("%-58s %9.1f ns\n", "  shaper overhead per write+read pair", mine - raw);
    bench_ns("unshaped socket: send gate + pace, no syscall", 4 * n, b_unshaped_gate);
    bench_ns("getsockopt(SO_TYPE) (what each send used to pay)", n, b_getsockopt);

    // Shaped paths without the syscalls: a shaped TCP socket's send gate and pacing (no limits set),
    // and a UDP send to a remote destination under 8 CIDR/name rules.
    set_conf("rule 10.0.0.0/8 latency_ms=1\nrule 2001:db8::/32 latency_ms=1\nrule a.test latency_ms=1\nrule b.test latency_ms=1\n"
             "rule 172.16.0.0/12 latency_ms=1\nrule c.test latency_ms=1\nrule 192.168.0.0/16 latency_ms=1\nrule d.test latency_ms=1\n");
    b_tcp = my_socket(AF_INET, SOCK_STREAM, 0);
    atomic_store(&g_fd[b_tcp], FD_SHAPED | FD_TYPED | FD_SEEN);
    b_udp = udp_socket(AF_INET);
    b_to = addr("198.51.100.7", 53);
    for (int i = 0; i < 64; i++) {
        char name[32];
        snprintf(name, sizeof name, "host%d.example", i);
        struct sockaddr_storage a = addr("192.0.2.1", 0);
        ((struct sockaddr_in *)&a)->sin_addr.s_addr = htonl(0xc0000200u + (unsigned)i);
        learn(name, (struct sockaddr *)&a);
    }
    atomic_store(&g_next_check, now_ns() + 3600LL * 1000000000);
    n = 2000000;
    bench_ns("effective(rule)", n, b_effective);
    bench_ns("shaped TCP: send gate + pace(send) + pace(recv)", n, b_gate_shaped_tcp);
    bench_ns("UDP sendto remote, 8 rules: gate (cached match)", n, b_gate_udp_to);
    bench_ns("UDP sendto remote, 8 rules: match_rule (uncached)", n / 4, b_match);
    my_close(b_tcp);
    my_close(b_udp);
    my_close(b_sp[0]);
    my_close(b_sp[1]);
}

/// Prints the effective profile for each host: `offline latency_ms down_bps up_bps loss_pct`.
static int print_effective(const char *path, char **hosts, int n) {
    setenv("SIMEE_NETSHAPE", path, 1);
    for (int i = 0; i < n; i++) {
        struct sockaddr_storage ss = addr(hosts[i], 443);
        int rule = ss.ss_family ? match_rule((struct sockaddr *)&ss) : match_rule_name(hosts[i]);
        profile_t p = effective(rule);
        printf("%s %d %d %ld %ld %d\n", hosts[i], p.offline, p.latency_ms, p.down_bps, p.up_bps, p.loss_pct);
    }
    return 0;
}

int main(int argc, char **argv) {
    if (argc >= 3 && !strcmp(argv[1], "effective")) return print_effective(argv[2], argv + 3, argc - 3);
    const char *tmp = getenv("TMPDIR");
    snprintf(conf_path, sizeof conf_path, "%s/netshape-test-%d.conf", tmp && *tmp ? tmp : "/tmp", getpid());
    setenv("SIMEE_NETSHAPE", conf_path, 1);
    if (argc >= 2 && !strcmp(argv[1], "bench")) {
        bench();
    } else {
        test_parse();
        test_reload_and_effective();
        test_match();
        test_fd_state();
        test_datagram_gate_and_cache();
        test_unshaped_paths();
        test_bucket();
        test_tcp_pacing();
        test_inspect_first();
        test_udp_delay();
        test_latch();
        test_dst_concurrency();
    }
    unlink(conf_path);
    if (failures) fprintf(stderr, "%d check(s) failed\n", failures);
    else printf("netshape core: all checks passed\n");
    return failures != 0;
}

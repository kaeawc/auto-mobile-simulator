// simee network shaper: a dylib injected into iOS-simulator processes (DYLD_INSERT_LIBRARIES).
// It interposes BSD socket calls and applies a profile read from the file named by
// SIMEE_NETSHAPE: offline, added latency, up/down bandwidth and connection loss. Only sockets
// connected to non-loopback addresses are shaped, so local control channels are unaffected.
// The profile is re-read when the file changes, so a controller can change it live.
//
// Hot paths. Every read, write, send and receive of every process of a boot-injected device goes
// through here, so an fd that isn't shaped costs one relaxed load of its state word, and a shaped
// socket's I/O takes no locks and makes no extra system calls:
// - Per-fd state is one atomic word (g_fd): shaped, socket type, matched rule and the TCP flags.
//   The type is read with getsockopt once, when a socket becomes shaped. Every close and fd
//   creation we can see (close, close$NOCANCEL, guarded_close_np, socket, accept, dup, dup2)
//   clears the word, so a reused fd number doesn't inherit an old socket's state.
// - The profile is published as a latch: two copies of every rule's effective profile and a
//   sequence number whose low bit names the stable copy. Readers never wait for the writer.
// - Rules are parsed once per reload (CIDRs to bytes and prefix lengths). The rule matched for an
//   unconnected socket's destination is cached per fd while the rules and learned names stay the
//   same (g_match_gen), so a stream of datagrams matches once.
// - Bandwidth token buckets are single atomics advanced by compare-and-swap.
// docs/research/netshape-overhead.md compares this with other interposers.
//
// NETSHAPE_HOST_TEST builds the portable core into a host program (native/netshape_test.c): no
// interposing and none of the Apple-only hooks.
#include <arpa/inet.h>
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <netdb.h>
#include <netinet/in.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <strings.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/uio.h>
#include <time.h>
#include <unistd.h>

#if defined(__APPLE__) && !defined(NETSHAPE_HOST_TEST)
#define NETSHAPE_INJECTED 1
#define DYLD_INTERPOSE(_replacement, _replacee)                                                  \
    __attribute__((used)) static struct {                                                        \
        const void *replacement;                                                                 \
        const void *replacee;                                                                    \
    } _interpose_##_replacee __attribute__((section("__DATA,__interpose"))) = {                  \
        (const void *)(unsigned long)&_replacement, (const void *)(unsigned long)&_replacee};
#else
#define DYLD_INTERPOSE(_replacement, _replacee)                                                  \
    __attribute__((used)) static const void *const _interpose_##_replacee = (const void *)(unsigned long)&_replacement;
#endif

#ifdef __APPLE__
#define MTIME(st) ((st).st_mtimespec)
#else
#define MTIME(st) ((st).st_mtim)
#endif
#ifndef NETSHAPE_RANDOM
#define NETSHAPE_RANDOM(n) arc4random_uniform(n)
#endif
#ifndef NETSHAPE_COUNT // host tests count rule matches and socket-type lookups
#define NETSHAPE_COUNT(what) ((void)0)
#endif

typedef struct {
    int offline;
    int latency_ms;
    long down_bps; // bytes per second, 0 = unlimited
    long up_bps;
    int loss_pct;  // TCP: percent of new connections that time out; UDP: percent of datagrams dropped
    int port_base; // >0: bind shaped sockets to source ports [port_base, port_base+port_count)
    int port_count;
} profile_t;

// A per-host override: `rule <pattern> key=value ...`. The pattern is a CIDR (`10.0.0.0/8`,
// `2001:db8::/32`), an address, a hostname suffix (`example.com` matches `api.example.com`), or `*`.
// Fields not given in the rule keep the base profile's values.
#define MAX_RULES 32
typedef struct {
    char pattern[128];
    int set_offline, offline, set_latency, latency_ms, set_down, set_up, set_loss, loss_pct;
    long down_bps, up_bps;
    // Parsed at reload: `*`, and the pattern as an address (family 0 = not one) with its prefix.
    int any, family, bits;
    unsigned char net[16];
} rule_t;

static int64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (int64_t)ts.tv_sec * 1000000000 + ts.tv_nsec;
}

static void sleep_ns(int64_t ns) {
    if (ns <= 0) return;
    struct timespec ts = {(time_t)(ns / 1000000000), (long)(ns % 1000000000)};
    while (nanosleep(&ts, &ts) == -1 && errno == EINTR) {}
}

static void sleep_s(double s) { sleep_ns((int64_t)(s * 1e9)); }

// ---- The profile file ----

typedef struct {
    profile_t base;
    rule_t rules[MAX_RULES];
    int nrules;
    char line[512]; // lines are cut at 511 bytes, as fgets with this buffer did
    size_t len;
} parser_t;

/// Parses a rule's pattern once: `*`, or an address with an optional prefix length (a missing or
/// out-of-range prefix means the whole address).
static void compile_rule(rule_t *r) {
    r->any = !strcmp(r->pattern, "*");
    char buf[128];
    snprintf(buf, sizeof buf, "%s", r->pattern);
    int bits = -1, len;
    char *slash = strchr(buf, '/');
    if (slash) { *slash = 0; bits = atoi(slash + 1); }
    if (inet_pton(AF_INET, buf, r->net) == 1) { r->family = AF_INET; len = 4; }
    else if (inet_pton(AF_INET6, buf, r->net) == 1) { r->family = AF_INET6; len = 16; }
    else { r->family = 0; return; }
    r->bits = bits < 0 || bits > len * 8 ? len * 8 : bits;
}

static void parse_line(parser_t *ps, char *line) {
    char key[64];
    long value;
    if (!strncmp(line, "rule ", 5)) {
        if (ps->nrules >= MAX_RULES) return;
        rule_t *r = &ps->rules[ps->nrules];
        memset(r, 0, sizeof *r);
        char *save = NULL, *tok = strtok_r(line + 5, " \t\r\n", &save);
        if (!tok) return;
        snprintf(r->pattern, sizeof r->pattern, "%s", tok);
        while ((tok = strtok_r(NULL, " \t\r\n", &save))) {
            if (sscanf(tok, "%63[^=]=%ld", key, &value) != 2) continue;
            if (!strcmp(key, "offline")) { r->set_offline = 1; r->offline = (int)value; }
            else if (!strcmp(key, "latency_ms")) { r->set_latency = 1; r->latency_ms = (int)value; }
            else if (!strcmp(key, "down_kbps")) { r->set_down = 1; r->down_bps = value * 1000 / 8; }
            else if (!strcmp(key, "up_kbps")) { r->set_up = 1; r->up_bps = value * 1000 / 8; }
            else if (!strcmp(key, "loss_pct")) { r->set_loss = 1; r->loss_pct = (int)value; }
        }
        compile_rule(r);
        ps->nrules++;
        return;
    }
    profile_t *p = &ps->base;
    if (sscanf(line, " %63[^= ] = %ld", key, &value) != 2) return;
    if (!strcmp(key, "offline")) p->offline = (int)value;
    else if (!strcmp(key, "latency_ms")) p->latency_ms = (int)value;
    else if (!strcmp(key, "down_kbps")) p->down_bps = value * 1000 / 8;
    else if (!strcmp(key, "up_kbps")) p->up_bps = value * 1000 / 8;
    else if (!strcmp(key, "loss_pct")) p->loss_pct = (int)value;
    else if (!strcmp(key, "port_base")) p->port_base = (int)value;
    else if (!strcmp(key, "port_count")) p->port_count = (int)value;
}

static void parse_begin(parser_t *ps) { memset(ps, 0, sizeof *ps); }

static void parse_feed(parser_t *ps, const char *buf, size_t n) {
    for (size_t i = 0; i < n; i++) {
        ps->line[ps->len++] = buf[i];
        if (buf[i] == '\n' || ps->len == sizeof ps->line - 1) {
            ps->line[ps->len] = 0;
            parse_line(ps, ps->line);
            ps->len = 0;
        }
    }
}

static void parse_end(parser_t *ps) {
    if (!ps->len) return;
    ps->line[ps->len] = 0;
    parse_line(ps, ps->line);
    ps->len = 0;
}

static profile_t apply_rule(profile_t p, const rule_t *r) {
    if (r->set_offline) p.offline = r->offline;
    if (r->set_latency) p.latency_ms = r->latency_ms;
    if (r->set_down) p.down_bps = r->down_bps;
    if (r->set_up) p.up_bps = r->up_bps;
    if (r->set_loss) p.loss_pct = r->loss_pct;
    return p;
}

// ---- The live profile ----

// Every rule's effective profile, published as a latch: two copies and a sequence number whose low
// bit names the copy readers use. The (serialized) writer updates the other copy, flips, then
// updates the first, so a reader retries only when a publish overlapped it and never waits for a
// writer. Fields are relaxed atomics, so a read that's retried isn't a data race.
typedef struct {
    _Atomic int offline, latency_ms, loss_pct, port_base, port_count;
    _Atomic long down_bps, up_bps;
} shared_profile_t;
typedef struct {
    _Atomic int nrules;
    shared_profile_t eff[MAX_RULES + 1]; // [0] the base profile, [i] the base with rule i applied
} published_t;
static published_t g_pub[2];
static _Atomic unsigned g_pub_seq;

// Matching reads the rules under g_lock; only the reloading thread writes them.
static pthread_mutex_t g_lock = PTHREAD_MUTEX_INITIALIZER;
static rule_t g_rules[MAX_RULES];
static int g_nrules;
static _Atomic int g_nrules_hint; // g_nrules, readable without the lock (0: nothing to match)
// Bumped whenever a match could come out differently (new rules, a newly learned name).
static _Atomic unsigned g_match_gen;

static pthread_mutex_t g_reload_lock = PTHREAD_MUTEX_INITIALIZER; // one thread reads the file
#define CHECK_NS 250000000LL
static _Atomic int64_t g_next_check;
static _Atomic int g_loaded;
static struct timespec g_mtime;
static profile_t g_base;

static void publish(const profile_t *base, const rule_t *rules, int nrules) {
    profile_t eff[MAX_RULES + 1];
    eff[0] = *base;
    for (int i = 0; i < nrules; i++) eff[i + 1] = apply_rule(*base, &rules[i]);
    for (int copy = 0; copy < 2; copy++) {
        // Readers move to the other copy (and see its previous writes) before this one changes.
        atomic_fetch_add_explicit(&g_pub_seq, 1, memory_order_release);
        atomic_thread_fence(memory_order_release);
        published_t *pb = &g_pub[copy];
        atomic_store_explicit(&pb->nrules, nrules, memory_order_relaxed);
        for (int i = 0; i <= nrules; i++) {
            shared_profile_t *s = &pb->eff[i];
            atomic_store_explicit(&s->offline, eff[i].offline, memory_order_relaxed);
            atomic_store_explicit(&s->latency_ms, eff[i].latency_ms, memory_order_relaxed);
            atomic_store_explicit(&s->loss_pct, eff[i].loss_pct, memory_order_relaxed);
            atomic_store_explicit(&s->port_base, eff[i].port_base, memory_order_relaxed);
            atomic_store_explicit(&s->port_count, eff[i].port_count, memory_order_relaxed);
            atomic_store_explicit(&s->down_bps, eff[i].down_bps, memory_order_relaxed);
            atomic_store_explicit(&s->up_bps, eff[i].up_bps, memory_order_relaxed);
        }
    }
}

/// Makes a parsed profile live (the caller holds g_reload_lock).
static void install(const parser_t *ps) {
    pthread_mutex_lock(&g_lock);
    memcpy(g_rules, ps->rules, sizeof(rule_t) * (size_t)ps->nrules);
    g_nrules = ps->nrules;
    atomic_store_explicit(&g_nrules_hint, ps->nrules, memory_order_relaxed);
    pthread_mutex_unlock(&g_lock);
    g_base = ps->base;
    publish(&g_base, g_rules, g_nrules);
    atomic_fetch_add_explicit(&g_match_gen, 1, memory_order_release);
}

/// Re-reads the profile file if it changed (the caller holds g_reload_lock). Reads with open/read,
/// so a reload doesn't allocate.
static void check_profile(void) {
    const char *path = getenv("SIMEE_NETSHAPE");
    struct stat st;
    if (!path || stat(path, &st) != 0) {
        // No file: the base profile is cleared, the rules stay.
        static const profile_t none;
        if (g_base.offline || g_base.latency_ms || g_base.down_bps || g_base.up_bps || g_base.loss_pct || g_base.port_base ||
            g_base.port_count) {
            g_base = none;
            publish(&g_base, g_rules, g_nrules);
        }
        return;
    }
    if (MTIME(st).tv_sec == g_mtime.tv_sec && MTIME(st).tv_nsec == g_mtime.tv_nsec) return;
    g_mtime = MTIME(st);
    static parser_t ps;
    parse_begin(&ps);
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd >= 0) {
        char buf[1024];
        ssize_t n;
        while ((n = read(fd, buf, sizeof buf)) > 0 || (n < 0 && errno == EINTR))
            if (n > 0) parse_feed(&ps, buf, (size_t)n);
        close(fd);
        parse_end(&ps);
    }
    install(&ps);
}

/// Picks up profile changes, checking the file at most every 250 ms. After the first load the
/// check is one clock read; one thread reads the file while the others keep the published profile.
static void reload(void) {
    int64_t t = now_ns(), next = atomic_load_explicit(&g_next_check, memory_order_relaxed);
    if (t < next) return;
    if (atomic_load_explicit(&g_loaded, memory_order_acquire)) {
        if (!atomic_compare_exchange_strong_explicit(&g_next_check, &next, t + CHECK_NS, memory_order_relaxed, memory_order_relaxed))
            return;
        if (pthread_mutex_trylock(&g_reload_lock) != 0) return;
        check_profile();
        pthread_mutex_unlock(&g_reload_lock);
        return;
    }
    // The first load: wait for it, so the first connection in a process sees the profile.
    pthread_mutex_lock(&g_reload_lock);
    if (!atomic_load_explicit(&g_loaded, memory_order_relaxed)) {
        atomic_store_explicit(&g_next_check, t + CHECK_NS, memory_order_relaxed);
        check_profile();
        atomic_store_explicit(&g_loaded, 1, memory_order_release);
    }
    pthread_mutex_unlock(&g_reload_lock);
}

/// The profile in effect under `rule` (index + 1, 0 = none): the base profile with that rule applied.
static profile_t effective(int rule) {
    reload();
    for (;;) {
        unsigned seq = atomic_load_explicit(&g_pub_seq, memory_order_acquire);
        const published_t *pb = &g_pub[seq & 1];
        int n = atomic_load_explicit(&pb->nrules, memory_order_relaxed);
        const shared_profile_t *s = &pb->eff[rule > 0 && rule <= n ? rule : 0];
        profile_t p;
        p.offline = atomic_load_explicit(&s->offline, memory_order_relaxed);
        p.latency_ms = atomic_load_explicit(&s->latency_ms, memory_order_relaxed);
        p.loss_pct = atomic_load_explicit(&s->loss_pct, memory_order_relaxed);
        p.port_base = atomic_load_explicit(&s->port_base, memory_order_relaxed);
        p.port_count = atomic_load_explicit(&s->port_count, memory_order_relaxed);
        p.down_bps = atomic_load_explicit(&s->down_bps, memory_order_relaxed);
        p.up_bps = atomic_load_explicit(&s->up_bps, memory_order_relaxed);
        atomic_thread_fence(memory_order_acquire);
        if (atomic_load_explicit(&g_pub_seq, memory_order_relaxed) == seq) return p;
    }
}

// ---- Matching ----

// Hostnames learned from this process's DNS lookups, so hostname rules can match addresses.
#define MAX_NAMES 256
typedef struct {
    struct sockaddr_storage addr;
    char name[128];
} learned_t;
static learned_t g_names[MAX_NAMES];
static int g_nnames;
static pthread_mutex_t g_names_lock = PTHREAD_MUTEX_INITIALIZER;

static int same_addr(const struct sockaddr *a, const struct sockaddr_storage *b) {
    if (a->sa_family != b->ss_family) return 0;
    if (a->sa_family == AF_INET)
        return ((const struct sockaddr_in *)a)->sin_addr.s_addr == ((const struct sockaddr_in *)b)->sin_addr.s_addr;
    if (a->sa_family == AF_INET6)
        return !memcmp(&((const struct sockaddr_in6 *)a)->sin6_addr, &((const struct sockaddr_in6 *)b)->sin6_addr, 16);
    return 0;
}

/// Records that `name` resolved to `sa` (most recent wins; a small ring).
static void learn(const char *name, const struct sockaddr *sa) {
    if (!name || !sa || (sa->sa_family != AF_INET && sa->sa_family != AF_INET6)) return;
    char clean[128];
    snprintf(clean, sizeof clean, "%s", name);
    size_t len = strlen(clean);
    if (len > 1 && clean[len - 1] == '.') clean[len - 1] = 0; // resolvers return FQDNs
    pthread_mutex_lock(&g_names_lock);
    learned_t *slot = &g_names[g_nnames % MAX_NAMES];
    for (int i = 0; i < MAX_NAMES && i < g_nnames; i++)
        if (same_addr(sa, &g_names[i].addr)) {
            slot = &g_names[i];
            if (!strcmp(slot->name, clean)) { // already known: matches cached under it stay valid
                pthread_mutex_unlock(&g_names_lock);
                return;
            }
            goto fill;
        }
    g_nnames++;
fill:
    memset(&slot->addr, 0, sizeof slot->addr);
    memcpy(&slot->addr, sa, sa->sa_family == AF_INET ? sizeof(struct sockaddr_in) : sizeof(struct sockaddr_in6));
    memcpy(slot->name, clean, sizeof clean);
    pthread_mutex_unlock(&g_names_lock);
    atomic_fetch_add_explicit(&g_match_gen, 1, memory_order_release);
}

static int name_of(const struct sockaddr *sa, char *out, size_t len) {
    int found = 0;
    pthread_mutex_lock(&g_names_lock);
    for (int i = 0; i < MAX_NAMES && i < g_nnames; i++)
        if (same_addr(sa, &g_names[i].addr)) { snprintf(out, len, "%s", g_names[i].name); found = 1; break; }
    pthread_mutex_unlock(&g_names_lock);
    return found;
}

static int suffix_match(const char *name, const char *pattern) {
    size_t n = strlen(name), p = strlen(pattern);
    if (p > n) return 0;
    if (strcasecmp(name + n - p, pattern)) return 0;
    return n == p || name[n - p - 1] == '.';
}

/// Whether `sa` is inside the rule's address or CIDR.
static int net_match(const struct sockaddr *sa, const rule_t *r) {
    if (!r->family || sa->sa_family != r->family) return 0;
    const unsigned char *have = r->family == AF_INET ? (const unsigned char *)&((const struct sockaddr_in *)sa)->sin_addr
                                                     : (const unsigned char *)&((const struct sockaddr_in6 *)sa)->sin6_addr;
    int whole = r->bits / 8, rest = r->bits % 8;
    if (memcmp(have, r->net, (size_t)whole)) return 0;
    return !rest || !((have[whole] ^ r->net[whole]) & (0xff00 >> rest) & 0xff);
}

/// Index + 1 of the first rule matching the peer `sa`, or 0.
static int match_rule(const struct sockaddr *sa) {
    if (!sa) return 0;
    reload(); // the first connection in a process may come before anything else loaded the rules
    if (!atomic_load_explicit(&g_nrules_hint, memory_order_relaxed)) return 0;
    NETSHAPE_COUNT(matches);
    char name[128];
    int named = -1, hit = 0;
    pthread_mutex_lock(&g_lock);
    for (int i = 0; i < g_nrules && !hit; i++) {
        const rule_t *r = &g_rules[i];
        if (r->any || net_match(sa, r)) { hit = i + 1; break; }
        if (named < 0) named = name_of(sa, name, sizeof name); // looked up once, when a rule needs it
        if (named && suffix_match(name, r->pattern)) hit = i + 1;
    }
    pthread_mutex_unlock(&g_lock);
    return hit;
}

/// Index + 1 of the first rule matching a hostname (for DNS lookups), or 0.
static int match_rule_name(const char *name) {
    if (!name) return 0;
    reload();
    if (!atomic_load_explicit(&g_nrules_hint, memory_order_relaxed)) return 0;
    char clean[128];
    snprintf(clean, sizeof clean, "%s", name);
    size_t len = strlen(clean);
    if (len > 1 && clean[len - 1] == '.') clean[len - 1] = 0;
    pthread_mutex_lock(&g_lock);
    int hit = 0;
    for (int i = 0; i < g_nrules && !hit; i++)
        if (g_rules[i].any || suffix_match(clean, g_rules[i].pattern)) hit = i + 1;
    pthread_mutex_unlock(&g_lock);
    return hit;
}

// The last destination matched on each fd (slots shared modulo DST_SLOTS). An entry maps an address
// to its rule at a g_match_gen, so it's valid for any fd while that generation lasts. A per-entry
// sequence number (odd while written) lets readers detect a concurrent write; a writer that finds
// the entry busy skips caching rather than wait.
#define DST_SLOTS 256
typedef struct {
    _Atomic unsigned seq, gen;
    _Atomic uint32_t key[5]; // family, then the address
    _Atomic int rule;
} dst_t;
static dst_t g_dst[DST_SLOTS];

static int dst_key(const struct sockaddr *sa, uint32_t key[5]) {
    memset(key, 0, 5 * sizeof *key);
    key[0] = sa->sa_family;
    if (sa->sa_family == AF_INET) memcpy(&key[1], &((const struct sockaddr_in *)sa)->sin_addr, 4);
    else if (sa->sa_family == AF_INET6) memcpy(&key[1], &((const struct sockaddr_in6 *)sa)->sin6_addr, 16);
    else return 0;
    return 1;
}

/// The cached rule for `key` at generation `gen`, or -1.
static int dst_lookup(int fd, const uint32_t key[5], unsigned gen) {
    dst_t *d = &g_dst[(unsigned)fd % DST_SLOTS];
    unsigned seq = atomic_load_explicit(&d->seq, memory_order_acquire);
    if (seq & 1) return -1;
    int same = atomic_load_explicit(&d->gen, memory_order_relaxed) == gen;
    for (int i = 0; i < 5; i++) same &= atomic_load_explicit(&d->key[i], memory_order_relaxed) == key[i];
    int rule = atomic_load_explicit(&d->rule, memory_order_relaxed);
    atomic_thread_fence(memory_order_acquire);
    return same && atomic_load_explicit(&d->seq, memory_order_relaxed) == seq ? rule : -1;
}

static void dst_store(int fd, const uint32_t key[5], unsigned gen, int rule) {
    dst_t *d = &g_dst[(unsigned)fd % DST_SLOTS];
    unsigned seq = atomic_load_explicit(&d->seq, memory_order_relaxed);
    if ((seq & 1) || !atomic_compare_exchange_strong_explicit(&d->seq, &seq, seq + 1, memory_order_relaxed, memory_order_relaxed))
        return;
    atomic_thread_fence(memory_order_release);
    atomic_store_explicit(&d->gen, gen, memory_order_relaxed);
    for (int i = 0; i < 5; i++) atomic_store_explicit(&d->key[i], key[i], memory_order_relaxed);
    atomic_store_explicit(&d->rule, rule, memory_order_relaxed);
    atomic_store_explicit(&d->seq, seq + 2, memory_order_release);
}

// ---- Per-fd state ----

#define MAX_FD 8192
enum {
    FD_SHAPED = 1u << 0, // a socket to a non-loopback peer
    FD_SENT = 1u << 1,   // TCP: the last operation was a send (the next receive owes a round trip)
    FD_SEEN = 1u << 2,   // TCP: the first outgoing bytes were inspected
    FD_TSTAMP = 1u << 3, // UDP: SO_TIMESTAMP enabled by us
    FD_DEBT = 1u << 4,   // TCP: handshake round trip owed by the first receive
    FD_TYPED = 1u << 5,  // FD_DGRAM is known (always, once shaped)
    FD_DGRAM = 1u << 6,  // SOCK_DGRAM
    FD_RULE_SHIFT = 8,   // bits 8-15: the peer's matching rule index + 1, 0 = none
    FD_RULE = 0xffu << FD_RULE_SHIFT,
};
static _Atomic uint32_t g_fd[MAX_FD];
// libSystem's own socket calls can call each other (recv -> recvfrom); count only the outermost.
static __thread int g_depth;

static uint32_t fd_get(int fd) { return fd >= 0 && fd < MAX_FD ? atomic_load_explicit(&g_fd[fd], memory_order_relaxed) : 0; }

static int fd_rule(uint32_t st) { return (int)((st & FD_RULE) >> FD_RULE_SHIFT); }

/// Clears then sets bits of an fd's state, writing only if that changes it. Returns the old state.
static uint32_t fd_update(int fd, uint32_t clear, uint32_t set) {
    if (fd < 0 || fd >= MAX_FD) return 0;
    uint32_t old = atomic_load_explicit(&g_fd[fd], memory_order_relaxed);
    while (((old & ~clear) | set) != old &&
           !atomic_compare_exchange_weak_explicit(&g_fd[fd], &old, (old & ~clear) | set, memory_order_relaxed, memory_order_relaxed)) {}
    return old;
}

/// A closed or newly created fd starts unshaped.
static void fd_reset(int fd) {
    if (fd >= 0 && fd < MAX_FD) atomic_store_explicit(&g_fd[fd], 0, memory_order_relaxed);
}

static uint32_t sock_type(int fd) {
    NETSHAPE_COUNT(sockopts);
    int type = 0;
    socklen_t len = sizeof type;
    return FD_TYPED | (getsockopt(fd, SOL_SOCKET, SO_TYPE, &type, &len) == 0 && type == SOCK_DGRAM ? FD_DGRAM : 0);
}

static int is_remote(const struct sockaddr *sa) {
    if (!sa) return 0;
    if (sa->sa_family == AF_INET) {
        const struct sockaddr_in *in = (const struct sockaddr_in *)sa;
        return (ntohl(in->sin_addr.s_addr) >> 24) != 127;
    }
    if (sa->sa_family == AF_INET6) {
        const struct sockaddr_in6 *in6 = (const struct sockaddr_in6 *)sa;
        if (IN6_IS_ADDR_LOOPBACK(&in6->sin6_addr)) return 0;
        if (IN6_IS_ADDR_V4MAPPED(&in6->sin6_addr)) return in6->sin6_addr.s6_addr[12] != 127;
        return 1;
    }
    return 0;
}

/// Binds an unbound socket to a free source port in the profile's range, so host packet filters
/// (pf + dummynet) can shape exactly this app's traffic. Best effort: failures leave it unbound.
static void bind_port(int fd, const struct sockaddr *dst, const profile_t *p) {
    if (p->port_base <= 0) return;
    int count = p->port_count > 0 ? p->port_count : 1000;
    static _Atomic unsigned next;
    for (int i = 0; i < count; i++) {
        unsigned port = (unsigned)p->port_base + (atomic_fetch_add(&next, 1) % (unsigned)count);
        struct sockaddr_storage ss;
        memset(&ss, 0, sizeof ss);
        socklen_t len;
        if (dst->sa_family == AF_INET6) {
            struct sockaddr_in6 *a = (struct sockaddr_in6 *)&ss;
            a->sin6_family = AF_INET6;
#ifdef SIN6_LEN
            a->sin6_len = sizeof *a;
#endif
            a->sin6_port = htons((uint16_t)port);
            len = sizeof *a;
        } else {
            struct sockaddr_in *a = (struct sockaddr_in *)&ss;
            a->sin_family = AF_INET;
#ifdef SIN6_LEN
            a->sin_len = sizeof *a;
#endif
            a->sin_port = htons((uint16_t)port);
            len = sizeof *a;
        }
        if (bind(fd, (struct sockaddr *)&ss, len) == 0) return;
        if (errno != EADDRINUSE) return; // already bound, or not bindable: leave it
    }
}

/// Gate for a new connection to `sa`: returns 0 to proceed, or -1 with errno set.
static int admit(int fd, const struct sockaddr *sa) {
    if (!is_remote(sa)) {
        fd_update(fd, ~(uint32_t)FD_TSTAMP, 0);
        return 0;
    }
    int rule = match_rule(sa);
    uint32_t type = sock_type(fd); // read afresh at every remote connect
    fd_update(fd, ~(uint32_t)FD_TSTAMP, FD_SHAPED | type | (uint32_t)rule << FD_RULE_SHIFT);
    profile_t p = effective(rule);
    if (p.offline) {
        errno = ENETUNREACH;
        return -1;
    }
    if (type & FD_DGRAM) return 0; // UDP "connect" only sets the peer; datagrams are shaped one by one
    // TCP does not lose connections, it retransmits: each lost SYN costs the initial retransmission
    // timeout (1 s, doubling). Six losses in a row time the connection out, as the kernel would.
    double rto = 1.0;
    for (int tries = 0; p.loss_pct > 0 && NETSHAPE_RANDOM(100) < (unsigned)p.loss_pct; tries++) {
        if (tries == 6) {
            errno = ETIMEDOUT;
            return -1;
        }
        sleep_s(rto);
        rto *= 2;
    }
    bind_port(fd, sa, &p);
    // The handshake round trip, paid where a real connect would wait. (Deferring it for
    // non-blocking sockets made Network.framework report "ready" a round trip early.)
    sleep_s(p.latency_ms / 1000.0);
    return 0;
}

/// A send to remote `to` shapes its socket under `to`'s rule. Returns the fd's new state.
static uint32_t mark_remote(int fd, const struct sockaddr *to, uint32_t st) {
    reload();
    uint32_t key[5];
    unsigned gen = atomic_load_explicit(&g_match_gen, memory_order_acquire);
    int keyed = dst_key(to, key), rule = keyed ? dst_lookup(fd, key, gen) : -1;
    if (rule < 0) {
        rule = match_rule(to);
        if (keyed) dst_store(fd, key, gen, rule);
    }
    uint32_t clear = FD_SENT | FD_RULE, set = FD_SHAPED | (uint32_t)rule << FD_RULE_SHIFT;
    if (!(st & FD_SHAPED)) { // newly shaped: read its type
        clear |= FD_DGRAM;
        set |= sock_type(fd);
    }
    return (fd_update(fd, clear, set) & ~clear) | set;
}

/// Extracts the TLS SNI hostname from a ClientHello record. Returns 1 on success.
static int tls_sni(const unsigned char *b, size_t n, char *out, size_t outlen) {
    if (n < 43 || b[0] != 0x16 || b[1] != 0x03 || b[5] != 0x01) return 0; // handshake record, ClientHello
    size_t i = 5 + 4 + 2 + 32;                                             // header, hs header, version, random
    if (i >= n) return 0;
    i += 1 + b[i];                                                          // session id
    if (i + 2 > n) return 0;
    i += 2 + ((size_t)b[i] << 8 | b[i + 1]);                                // cipher suites
    if (i + 1 > n) return 0;
    i += 1 + b[i];                                                          // compression methods
    if (i + 2 > n) return 0;
    size_t end = i + 2 + ((size_t)b[i] << 8 | b[i + 1]);
    i += 2;
    while (i + 4 <= n && i + 4 <= end) {
        unsigned type = (unsigned)b[i] << 8 | b[i + 1];
        size_t len = (size_t)b[i + 2] << 8 | b[i + 3];
        i += 4;
        if (type == 0 && i + 5 <= n) {                                      // server_name
            size_t name_len = (size_t)b[i + 3] << 8 | b[i + 4];
            if (b[i + 2] != 0 || i + 5 + name_len > n || name_len >= outlen) return 0;
            memcpy(out, b + i + 5, name_len);
            out[name_len] = 0;
            return 1;
        }
        i += len;
    }
    return 0;
}

/// Extracts the Host header from a plain HTTP request. Returns 1 on success.
static int http_host(const unsigned char *b, size_t n, char *out, size_t outlen) {
    static const char *methods[] = {"GET ", "POST ", "PUT ", "HEAD ", "DELETE ", "PATCH ", "OPTIONS "};
    int is_http = 0;
    for (size_t m = 0; m < sizeof methods / sizeof *methods; m++)
        if (n > strlen(methods[m]) && !memcmp(b, methods[m], strlen(methods[m]))) is_http = 1;
    if (!is_http) return 0;
    for (size_t i = 0; i + 6 < n; i++) {
        if ((b[i] == '\n') && (b[i + 1] == 'H' || b[i + 1] == 'h') && !strncasecmp((const char *)b + i + 1, "host:", 5)) {
            size_t j = i + 6, k = 0;
            while (j < n && b[j] == ' ') j++;
            while (j < n && b[j] != '\r' && b[j] != '\n' && b[j] != ':' && k + 1 < outlen) out[k++] = (char)b[j++];
            out[k] = 0;
            return k > 0;
        }
    }
    return 0;
}

/// Inspects the first outgoing bytes of a shaped TCP socket (state `st`) for its hostname (TLS SNI
/// or HTTP Host), learns it and re-matches rules. Returns -1 with errno set if a hostname rule makes
/// it offline.
static int inspect_first(int fd, uint32_t st, const void *buf, size_t n) {
    if ((st & (FD_SHAPED | FD_DGRAM | FD_SEEN)) != FD_SHAPED || !buf || n == 0 || (fd_update(fd, 0, FD_SEEN) & FD_SEEN)) return 0;
    char name[128];
    if (!tls_sni(buf, n, name, sizeof name) && !http_host(buf, n, name, sizeof name)) return 0;
    struct sockaddr_storage peer;
    socklen_t len = sizeof peer;
    if (getpeername(fd, (struct sockaddr *)&peer, &len) != 0) return 0;
    learn(name, (struct sockaddr *)&peer);
    int rule = match_rule((struct sockaddr *)&peer);
    int before = fd_rule(fd_update(fd, FD_RULE, (uint32_t)rule << FD_RULE_SHIFT));
    profile_t p = effective(rule);
    if (p.offline) {
        errno = ECONNRESET;
        return -1;
    }
    // The handshake ran under the previous match; charge its round trip if the name changed it.
    if (rule != before) fd_update(fd, 0, FD_DEBT);
    return 0;
}

/// Gate for outgoing bytes on `fd` (to `to`, when given): a datagram on a shaped socket may be
/// dropped (returns 1: report it sent) or refused offline (-1, errno set); the first bytes of a
/// shaped stream are inspected for its hostname (-1 if that makes it offline). 0 sends.
static int send_gate(int fd, const struct sockaddr *to, const void *buf, size_t n) {
    uint32_t st = fd_get(fd);
    if (to && fd >= 0 && fd < MAX_FD && is_remote(to)) st = mark_remote(fd, to, st);
    if (!(st & FD_SHAPED)) return 0;
    if (!(st & FD_DGRAM)) return inspect_first(fd, st, buf, n);
    profile_t p = effective(fd_rule(st));
    if (p.offline) {
        errno = ENETUNREACH;
        return -1;
    }
    if (p.loss_pct > 0 && NETSHAPE_RANDOM(100) < (unsigned)p.loss_pct) return 1;
    return 0; // latency is charged on the receive side (see deliver_at)
}

static double wall_s(void) {
    struct timeval tv;
    gettimeofday(&tv, NULL);
    return tv.tv_sec + tv.tv_usec / 1e6;
}

/// UDP delay line: a datagram becomes visible at its kernel arrival time plus the latency. Datagrams
/// that arrived together are released together, and request/response pays exactly one round trip.
/// Returns 1 if the socket is a shaped datagram socket with latency (so callers want timestamps).
static int udp_delayed(int fd, profile_t *out) {
    uint32_t st = fd_get(fd);
    if ((st & (FD_SHAPED | FD_DGRAM)) != (FD_SHAPED | FD_DGRAM)) return 0;
    profile_t p = effective(fd_rule(st));
    if (p.latency_ms <= 0) return 0;
    if (!(st & FD_TSTAMP) && !(fd_update(fd, 0, FD_TSTAMP) & FD_TSTAMP)) {
        int on = 1;
        setsockopt(fd, SOL_SOCKET, SO_TIMESTAMP, &on, sizeof on);
    }
    *out = p;
    return 1;
}

static void deliver_at(double arrival, const profile_t *p) {
    if (arrival <= 0) arrival = wall_s();
    sleep_s(arrival + p->latency_ms / 1000.0 - wall_s());
}

/// Kernel arrival time from an SCM_TIMESTAMP control message, or 0.
static double cmsg_arrival(const struct msghdr *m) {
    if (!m || !m->msg_control || m->msg_controllen < sizeof(struct cmsghdr)) return 0;
    for (struct cmsghdr *c = CMSG_FIRSTHDR(m); c; c = CMSG_NXTHDR((struct msghdr *)m, c)) {
        if (c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_TIMESTAMP) {
            struct timeval tv;
            memcpy(&tv, CMSG_DATA(c), sizeof tv);
            return tv.tv_sec + tv.tv_usec / 1e6;
        }
    }
    return 0;
}

/// recv/recvfrom on a delayed UDP socket, through recvmsg so the arrival timestamp is visible.
static ssize_t udp_recv(int fd, void *b, size_t n, int fl, struct sockaddr *from, socklen_t *fromlen, const profile_t *p) {
    struct iovec iov = {b, n};
    char control[256];
    struct msghdr m = {0};
    m.msg_name = from;
    m.msg_namelen = fromlen ? *fromlen : 0;
    m.msg_iov = &iov;
    m.msg_iovlen = 1;
    m.msg_control = control;
    m.msg_controllen = sizeof control;
    g_depth++;
    ssize_t r = recvmsg(fd, &m, fl);
    g_depth--;
    if (r >= 0) {
        if (fromlen) *fromlen = m.msg_namelen;
        if (!(fl & MSG_PEEK)) deliver_at(cmsg_arrival(&m), p);
    }
    return r;
}

// Token buckets per direction, shared by the process's shaped sockets: when the link is next free.
static _Atomic int64_t g_free_at[2]; // [0] down, [1] up

static void bucket_wait(_Atomic int64_t *free_at, ssize_t bytes, long rate) {
    int64_t t = now_ns(), cost = (int64_t)((double)bytes * 1e9 / (double)rate);
    int64_t prev = atomic_load_explicit(free_at, memory_order_relaxed), end;
    do end = (prev > t ? prev : t) + cost;
    while (!atomic_compare_exchange_weak_explicit(free_at, &prev, end, memory_order_relaxed, memory_order_relaxed));
    sleep_ns(end - t);
}

/// Accounts `bytes` on a shaped socket and sleeps as the profile requires.
static void pace(int fd, ssize_t bytes, int sending) {
    uint32_t st = fd_get(fd);
    if (!(st & FD_SHAPED) || bytes <= 0) return;
    profile_t p = effective(fd_rule(st));
    int dgram = (st & FD_DGRAM) != 0;
    // Request -> response turnaround: data received after this socket sent costs a round trip.
    // Exact for request/response protocols; TCP hides segment arrival times, so pipelined traffic
    // (HTTP/2 control frames sent mid-response) can be charged extra round trips. Use pf/dummynet
    // (scripts/pf-shape.sh) when that matters.
    if (!sending && !dgram && p.latency_ms > 0) {
        int owed = ((fd_update(fd, FD_DEBT, 0) & FD_DEBT) != 0) + ((st & FD_SENT) != 0);
        sleep_s(owed * p.latency_ms / 1000.0);
    }

    // A lost TCP segment stalls the stream for a retransmission (about 200 ms plus a round trip).
    if (!dgram && p.loss_pct > 0 && NETSHAPE_RANDOM(100) < (unsigned)p.loss_pct) sleep_s(0.2 + p.latency_ms / 1000.0);
    if (!dgram) fd_update(fd, sending ? 0 : FD_SENT, sending ? FD_SENT : 0);
    long rate = sending ? p.up_bps : p.down_bps;
    if (rate > 0) bucket_wait(&g_free_at[sending != 0], bytes, rate);
}

// ---- Interposed calls ----

static int my_connect(int fd, const struct sockaddr *sa, socklen_t len) {
    if (admit(fd, sa) != 0) return -1;
    return connect(fd, sa, len);
}
DYLD_INTERPOSE(my_connect, connect)

#define OUTER(call) (g_depth++, _r = (call), g_depth--, g_depth == 0)

static ssize_t my_send(int fd, const void *b, size_t n, int fl) {
    ssize_t _r;
    if (g_depth == 0) {
        int gate = send_gate(fd, NULL, b, n);
        if (gate < 0) return -1;
        if (gate > 0) return (ssize_t)n;
    }
    if (OUTER(send(fd, b, n, fl))) pace(fd, _r, 1);
    return _r;
}
DYLD_INTERPOSE(my_send, send)

static ssize_t my_sendto(int fd, const void *b, size_t n, int fl, const struct sockaddr *to, socklen_t tl) {
    ssize_t _r;
    if (g_depth == 0) {
        int gate = send_gate(fd, to, b, n);
        if (gate < 0) return -1;
        if (gate > 0) return (ssize_t)n;
    }
    if (OUTER(sendto(fd, b, n, fl, to, tl))) pace(fd, _r, 1);
    return _r;
}
DYLD_INTERPOSE(my_sendto, sendto)

static ssize_t iov_total(const struct iovec *v, int c) {
    ssize_t t = 0;
    for (int i = 0; v && i < c; i++) t += (ssize_t)v[i].iov_len;
    return t;
}

static ssize_t my_sendmsg(int fd, const struct msghdr *m, int fl) {
    ssize_t _r;
    if (g_depth == 0) {
        int first = m && m->msg_iovlen > 0;
        int gate = send_gate(fd, m ? (const struct sockaddr *)m->msg_name : NULL, first ? m->msg_iov[0].iov_base : NULL,
                             first ? m->msg_iov[0].iov_len : 0);
        if (gate < 0) return -1;
        if (gate > 0) return m ? iov_total(m->msg_iov, (int)m->msg_iovlen) : 0;
    }
    if (OUTER(sendmsg(fd, m, fl))) pace(fd, _r, 1);
    return _r;
}
DYLD_INTERPOSE(my_sendmsg, sendmsg)

static ssize_t my_recv(int fd, void *b, size_t n, int fl) {
    ssize_t _r;
    profile_t up;
    if (g_depth == 0 && udp_delayed(fd, &up)) return udp_recv(fd, b, n, fl, NULL, NULL, &up);
    if (OUTER(recv(fd, b, n, fl))) pace(fd, _r, 0);
    return _r;
}
DYLD_INTERPOSE(my_recv, recv)

static ssize_t my_recvfrom(int fd, void *b, size_t n, int fl, struct sockaddr *from, socklen_t *fl2) {
    ssize_t _r;
    profile_t up;
    if (g_depth == 0 && udp_delayed(fd, &up)) return udp_recv(fd, b, n, fl, from, fl2, &up);
    if (OUTER(recvfrom(fd, b, n, fl, from, fl2))) pace(fd, _r, 0);
    return _r;
}
DYLD_INTERPOSE(my_recvfrom, recvfrom)

static ssize_t my_recvmsg(int fd, struct msghdr *m, int fl) {
    ssize_t _r;
    profile_t up;
    int delayed = g_depth == 0 && udp_delayed(fd, &up);
    if (OUTER(recvmsg(fd, m, fl))) {
        if (delayed && _r >= 0 && !(fl & MSG_PEEK)) deliver_at(cmsg_arrival(m), &up);
        pace(fd, _r, 0);
    }
    return _r;
}
DYLD_INTERPOSE(my_recvmsg, recvmsg)

static ssize_t my_write(int fd, const void *b, size_t n) {
    ssize_t _r;
    if (g_depth == 0 && inspect_first(fd, fd_get(fd), b, n) < 0) return -1;
    if (OUTER(write(fd, b, n))) pace(fd, _r, 1);
    return _r;
}
DYLD_INTERPOSE(my_write, write)

static ssize_t my_read(int fd, void *b, size_t n) {
    ssize_t _r;
    if (OUTER(read(fd, b, n))) pace(fd, _r, 0);
    return _r;
}
DYLD_INTERPOSE(my_read, read)

static ssize_t my_writev(int fd, const struct iovec *v, int c) {
    ssize_t _r;
    if (g_depth == 0 && c > 0 && inspect_first(fd, fd_get(fd), v[0].iov_base, v[0].iov_len) < 0) return -1;
    if (OUTER(writev(fd, v, c))) pace(fd, _r, 1);
    return _r;
}
DYLD_INTERPOSE(my_writev, writev)

static ssize_t my_readv(int fd, const struct iovec *v, int c) {
    ssize_t _r;
    if (OUTER(readv(fd, v, c))) pace(fd, _r, 0);
    return _r;
}
DYLD_INTERPOSE(my_readv, readv)

// fd lifecycle: a closed fd's state is cleared before the number can be reused, and a new fd
// starts clear even if its number was closed somewhere we don't see.
static int my_close(int fd) {
    fd_reset(fd);
    return close(fd);
}
DYLD_INTERPOSE(my_close, close)

static int my_socket(int domain, int type, int protocol) {
    int fd = socket(domain, type, protocol);
    fd_reset(fd);
    return fd;
}
DYLD_INTERPOSE(my_socket, socket)

static int my_accept(int fd, struct sockaddr *addr, socklen_t *len) {
    int c = accept(fd, addr, len);
    fd_reset(c);
    return c;
}
DYLD_INTERPOSE(my_accept, accept)

static int my_dup(int fd) {
    int d = dup(fd);
    fd_reset(d);
    return d;
}
DYLD_INTERPOSE(my_dup, dup)

static int my_dup2(int fd, int to) {
    int r = dup2(fd, to);
    if (r >= 0 && fd != to) fd_reset(to);
    return r;
}
DYLD_INTERPOSE(my_dup2, dup2)

// ---- DNS: learn names (for hostname rules) and fail lookups while offline ----

static int is_literal(const char *node) {
    struct in_addr a4;
    struct in6_addr a6;
    return inet_pton(AF_INET, node, &a4) == 1 || inet_pton(AF_INET6, node, &a6) == 1;
}

/// A DNS lookup is one round trip on the shaped link, and fails while the host (or all) is offline.
static int my_getaddrinfo(const char *node, const char *service, const struct addrinfo *hints, struct addrinfo **res) {
    if (node && !is_literal(node)) {
        profile_t p = effective(match_rule_name(node));
        if (p.offline) return EAI_NONAME;
        sleep_s(p.latency_ms / 1000.0);
    }
    int r = getaddrinfo(node, service, hints, res);
    if (r == 0 && node)
        for (struct addrinfo *ai = *res; ai; ai = ai->ai_next) learn(node, ai->ai_addr);
    return r;
}
DYLD_INTERPOSE(my_getaddrinfo, getaddrinfo)

#ifdef NETSHAPE_INJECTED

static int my_connectx(int fd, const sa_endpoints_t *ep, sae_associd_t aid, unsigned int flags, const struct iovec *iov,
                       unsigned int iovcnt, size_t *len, sae_connid_t *cid) {
    if (ep && admit(fd, ep->sae_dstaddr) != 0) return -1;
    return connectx(fd, ep, aid, flags, iov, iovcnt, len, cid);
}
DYLD_INTERPOSE(my_connectx, connectx)

// Closes that bypass `close`: the non-cancellable variant and guarded fds (SPI in libsystem_kernel).
extern int close_nocancel(int) __asm("_close$NOCANCEL");
static int my_close_nocancel(int fd) {
    fd_reset(fd);
    return close_nocancel(fd);
}
DYLD_INTERPOSE(my_close_nocancel, close_nocancel)

typedef uint64_t guardid_t;
extern int guarded_close_np(int fd, const guardid_t *guard);
static int my_guarded_close_np(int fd, const guardid_t *guard) {
    fd_reset(fd);
    return guarded_close_np(fd, guard);
}
DYLD_INTERPOSE(my_guarded_close_np, guarded_close_np)

// Batch socket calls used by Network.framework (private, exported by libsystem_kernel).
struct msghdr_x {
    void *msg_name;
    socklen_t msg_namelen;
    struct iovec *msg_iov;
    int msg_iovlen;
    void *msg_control;
    socklen_t msg_controllen;
    int msg_flags;
    size_t msg_datalen;
};
extern ssize_t sendmsg_x(int, const struct msghdr_x *, unsigned int, int);
extern ssize_t recvmsg_x(int, const struct msghdr_x *, unsigned int, int);

static ssize_t datalen(const struct msghdr_x *m, ssize_t count) {
    ssize_t total = 0;
    for (ssize_t i = 0; i < count; i++) total += (ssize_t)m[i].msg_datalen;
    return total;
}

static ssize_t my_sendmsg_x(int fd, const struct msghdr_x *m, unsigned int cnt, int fl) {
    ssize_t _r;
    if (g_depth == 0 && cnt > 0) {
        int first = m[0].msg_iovlen > 0;
        int gate = send_gate(fd, (const struct sockaddr *)m[0].msg_name, first ? m[0].msg_iov[0].iov_base : NULL,
                             first ? m[0].msg_iov[0].iov_len : 0);
        if (gate < 0) return -1;
        if (gate > 0) return (ssize_t)cnt; // the whole batch is lost
    }
    if (OUTER(sendmsg_x(fd, m, cnt, fl)) && _r > 0 && (fd_get(fd) & FD_SHAPED)) {
        ssize_t total = 0;
        for (ssize_t i = 0; i < _r; i++) total += iov_total(m[i].msg_iov, m[i].msg_iovlen);
        pace(fd, total, 1);
    }
    return _r;
}
DYLD_INTERPOSE(my_sendmsg_x, sendmsg_x)

static ssize_t my_recvmsg_x(int fd, const struct msghdr_x *m, unsigned int cnt, int fl) {
    ssize_t _r;
    profile_t up;
    int delayed = g_depth == 0 && udp_delayed(fd, &up);
    if (OUTER(recvmsg_x(fd, m, cnt, fl)) && _r > 0) {
        if (delayed) {
            // The batch becomes visible when its newest datagram is due.
            double newest = 0;
            for (ssize_t i = 0; i < _r; i++) {
                struct msghdr h = {0};
                h.msg_control = m[i].msg_control;
                h.msg_controllen = m[i].msg_controllen;
                double a = cmsg_arrival(&h);
                if (a > newest) newest = a;
            }
            deliver_at(newest, &up);
        }
        pace(fd, datalen(m, _r), 0);
    }
    return _r;
}
DYLD_INTERPOSE(my_recvmsg_x, recvmsg_x)

// DNSServiceGetAddrInfo (dns_sd), used by CFNetwork/Network.framework resolvers in some paths.
typedef void *DNSServiceRef;
typedef void (*DNSServiceGetAddrInfoReply)(DNSServiceRef, uint32_t, uint32_t, int32_t, const char *, const struct sockaddr *, uint32_t, void *);
extern int32_t DNSServiceGetAddrInfo(DNSServiceRef *, uint32_t, uint32_t, uint32_t, const char *, DNSServiceGetAddrInfoReply, void *);
typedef struct {
    DNSServiceGetAddrInfoReply reply;
    void *context;
    double delay; // seconds to hold the first answer (one round trip)
} dns_ctx_t;
static void dns_reply(DNSServiceRef ref, uint32_t flags, uint32_t ifi, int32_t err, const char *host, const struct sockaddr *addr, uint32_t ttl, void *ctx) {
    dns_ctx_t *c = ctx;
    if (c->delay > 0) {
        sleep_s(c->delay);
        c->delay = 0;
    }
    if (err == 0) learn(host, addr);
    c->reply(ref, flags, ifi, err, host, addr, ttl, c->context);
}
#define kDNSServiceErr_NoSuchName (-65538)
/// Offline (for this name): the query fails up front, as with no network.
static int dns_blocked(const char *host, double *delay) {
    profile_t p = effective(match_rule_name(host));
    *delay = p.latency_ms / 1000.0;
    return p.offline;
}
static int32_t my_DNSServiceGetAddrInfo(DNSServiceRef *ref, uint32_t flags, uint32_t ifi, uint32_t proto, const char *host, DNSServiceGetAddrInfoReply reply, void *ctx) {
    double delay = 0;
    if (host && dns_blocked(host, &delay)) return kDNSServiceErr_NoSuchName;
    dns_ctx_t *c = malloc(sizeof *c); // lives as long as the query; small and rare
    if (!c || !reply) return DNSServiceGetAddrInfo(ref, flags, ifi, proto, host, reply, ctx);
    c->reply = reply;
    c->context = ctx;
    c->delay = delay;
    return DNSServiceGetAddrInfo(ref, flags, ifi, proto, host, dns_reply, c);
}
DYLD_INTERPOSE(my_DNSServiceGetAddrInfo, DNSServiceGetAddrInfo)

// The attribute-taking variant, which libnetwork (Network.framework, URLSession) uses.
extern int32_t DNSServiceGetAddrInfoEx(DNSServiceRef *, uint32_t, uint32_t, uint32_t, const char *, const void *, DNSServiceGetAddrInfoReply, void *);
static int32_t my_DNSServiceGetAddrInfoEx(DNSServiceRef *ref, uint32_t flags, uint32_t ifi, uint32_t proto, const char *host, const void *attr, DNSServiceGetAddrInfoReply reply, void *ctx) {
    double delay = 0;
    if (host && dns_blocked(host, &delay)) return kDNSServiceErr_NoSuchName;
    dns_ctx_t *c = malloc(sizeof *c);
    if (!c || !reply) return DNSServiceGetAddrInfoEx(ref, flags, ifi, proto, host, attr, reply, ctx);
    c->reply = reply;
    c->context = ctx;
    c->delay = delay;
    return DNSServiceGetAddrInfoEx(ref, flags, ifi, proto, host, attr, dns_reply, c);
}
DYLD_INTERPOSE(my_DNSServiceGetAddrInfoEx, DNSServiceGetAddrInfoEx)

// ---- Path status: while the whole profile is offline, the device has no network path ----
// NWPathMonitor, URLSession's connectivity checks and SCNetworkReachability then report
// "offline" (-1009) the way airplane mode does, instead of only failing connections.
typedef void *nw_path_t;
extern int nw_path_get_status(nw_path_t);
static int my_nw_path_get_status(nw_path_t path) {
    int status = nw_path_get_status(path);
    return effective(0).offline && status == 1 /* satisfied */ ? 2 /* unsatisfied */ : status;
}
DYLD_INTERPOSE(my_nw_path_get_status, nw_path_get_status)

typedef const void *SCNetworkReachabilityRef;
extern unsigned char SCNetworkReachabilityGetFlags(SCNetworkReachabilityRef, uint32_t *);
static unsigned char my_SCNetworkReachabilityGetFlags(SCNetworkReachabilityRef target, uint32_t *flags) {
    unsigned char ok = SCNetworkReachabilityGetFlags(target, flags);
    if (ok && flags && effective(0).offline) *flags = 0;
    return ok;
}
DYLD_INTERPOSE(my_SCNetworkReachabilityGetFlags, SCNetworkReachabilityGetFlags)

// dnssd_getaddrinfo (libdns_services), the resolver Network.framework and URLSession use. Every
// address handed to the client passes through this getter, so the name is learned before connect.
typedef void *dnssd_getaddrinfo_result_t;
extern const struct sockaddr *dnssd_getaddrinfo_result_get_address(dnssd_getaddrinfo_result_t);
extern const char *dnssd_getaddrinfo_result_get_hostname(dnssd_getaddrinfo_result_t);
static const struct sockaddr *my_dnssd_get_address(dnssd_getaddrinfo_result_t result) {
    const struct sockaddr *addr = dnssd_getaddrinfo_result_get_address(result);
    if (addr) learn(dnssd_getaddrinfo_result_get_hostname(result), addr);
    return addr;
}
DYLD_INTERPOSE(my_dnssd_get_address, dnssd_getaddrinfo_result_get_address)

__attribute__((constructor)) static void netshape_init(void) {
    const char *marker = getenv("SIMEE_NETSHAPE_MARK");
    if (marker) {
        int fd = open(marker, O_WRONLY | O_CREAT | O_APPEND, 0644);
        if (fd >= 0) {
            char line[256];
            int n = snprintf(line, sizeof line, "loaded pid=%d %s\n", getpid(), getprogname());
            write(fd, line, (size_t)n);
            close(fd);
        }
    }
}

#endif // NETSHAPE_INJECTED

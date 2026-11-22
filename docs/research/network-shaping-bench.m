// In-device network probe. Each command prints one line: "<kind> ok|err <detail>".
//   http <url>              URLSession GET: status, bytes, seconds, negotiated protocol
//   h3 <url>                same with assumesHTTP3Capable
//   ws <url> <n>            URLSessionWebSocketTask: n echo round trips, avg ms
//   tcp <ip> <port> <n>     BSD sockets: connect ms, avg RTT of n 64-byte echoes, 256 KB echo throughput
//   udp <ip> <port> <n>     BSD sockets: n datagrams, replies received, avg RTT
//   nwtcp <host> <port> <n> Network.framework TCP: connect ms, avg RTT of n echoes
//   nwudp <host> <port> <n> Network.framework UDP: n datagrams, replies, avg RTT
//   dns <name>              getaddrinfo time
#import <Foundation/Foundation.h>
#import <Network/Network.h>
#import <arpa/inet.h>
#import <netdb.h>
#import <sys/socket.h>
#import <sys/time.h>
@interface Metrics : NSObject <NSURLSessionTaskDelegate>
@property(copy) NSString *proto;
@end
@implementation Metrics
- (void)URLSession:(NSURLSession *)s task:(NSURLSessionTask *)t didFinishCollectingMetrics:(NSURLSessionTaskMetrics *)m {
    NSString *p = m.transactionMetrics.lastObject.networkProtocolName;
    BOOL proxy = m.transactionMetrics.lastObject.proxyConnection;
    self.proto = [NSString stringWithFormat:@"%@%@", p ?: @"?", proxy ? @"(via-proxy)" : @""];
}
@end
static double now(void) { struct timeval t; gettimeofday(&t, NULL); return t.tv_sec + t.tv_usec / 1e6; }
static void http(const char *u, BOOL h3) {
    NSURLSessionConfiguration *c = [NSURLSessionConfiguration ephemeralSessionConfiguration];
    c.timeoutIntervalForRequest = 20;
    Metrics *metrics = [Metrics new];
    NSURLSession *s = [NSURLSession sessionWithConfiguration:c delegate:metrics delegateQueue:nil];
    NSMutableURLRequest *r = [NSMutableURLRequest requestWithURL:[NSURL URLWithString:@(u)]];
    if (h3) r.assumesHTTP3Capable = YES;
    dispatch_semaphore_t sem = dispatch_semaphore_create(0);
    double t0 = now();
    NSURLSessionDataTask *task = [s dataTaskWithRequest:r completionHandler:^(NSData *d, NSURLResponse *res, NSError *e) {
        if (e) printf("%s err %ld %.2fs\n", h3 ? "h3" : "http", (long)e.code, now() - t0);
        else printf("%s ok status=%ld bytes=%lu %.2fs proto=%s\n", h3 ? "h3" : "http", (long)((NSHTTPURLResponse *)res).statusCode, (unsigned long)d.length, now() - t0, (metrics.proto ?: @"?").UTF8String);
        dispatch_semaphore_signal(sem);
    }];
    [task resume];
    dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 40 * NSEC_PER_SEC));
}
static void ws(const char *u, int n) {
    NSURLSession *s = [NSURLSession sessionWithConfiguration:[NSURLSessionConfiguration ephemeralSessionConfiguration]];
    NSURLSessionWebSocketTask *t = [s webSocketTaskWithURL:[NSURL URLWithString:@(u)]];
    [t resume];
    double total = 0; int ok = 0;
    double t0 = now();
    for (int i = 0; i < n; i++) {
        dispatch_semaphore_t sem = dispatch_semaphore_create(0);
        __block BOOL good = NO;
        double s0 = now();
        [t sendMessage:[[NSURLSessionWebSocketMessage alloc] initWithString:@"ping"] completionHandler:^(NSError *e) {
            if (e) { dispatch_semaphore_signal(sem); return; }
            [t receiveMessageWithCompletionHandler:^(NSURLSessionWebSocketMessage *m, NSError *e2) { good = (e2 == nil); dispatch_semaphore_signal(sem); }];
        }];
        if (dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 10 * NSEC_PER_SEC)) || !good) break;
        if (i > 0) total += now() - s0; // first round trip includes the handshake
        ok++;
    }
    if (ok == 0) printf("ws err no echo %.2fs\n", now() - t0);
    else printf("ws ok echoes=%d/%d avg_rtt=%.0fms total=%.2fs\n", ok, n, ok > 1 ? total * 1000 / (ok - 1) : 0, now() - t0);
    [t cancel];
}
static int dial(const char *ip, int port, int type) {
    struct sockaddr_in a = {.sin_len = sizeof a, .sin_family = AF_INET, .sin_port = htons(port)};
    inet_pton(AF_INET, ip, &a.sin_addr);
    int fd = socket(AF_INET, type, 0);
    struct timeval tv = {3, 0};
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    if (connect(fd, (struct sockaddr *)&a, sizeof a) != 0) { close(fd); return -1; }
    return fd;
}
static void tcp(const char *ip, int port, int n) {
    double t0 = now();
    int fd = dial(ip, port, SOCK_STREAM);
    if (fd < 0) { printf("tcp err connect errno=%d %.2fs\n", errno, now() - t0); return; }
    double conn = now() - t0, rtt = 0;
    char buf[65536] = {0};
    for (int i = 0; i < n; i++) {
        double s0 = now();
        send(fd, buf, 64, 0);
        ssize_t got = 0;
        while (got < 64) { ssize_t r = recv(fd, buf, 64 - got, 0); if (r <= 0) { printf("tcp err echo\n"); close(fd); return; } got += r; }
        rtt += now() - s0;
    }
    size_t total = 256 * 1024, sent = 0, got = 0;
    double b0 = now();
    while (got < total) {
        if (sent < total) { size_t k = total - sent > 16384 ? 16384 : total - sent; ssize_t w = send(fd, buf, k, 0); if (w > 0) sent += (size_t)w; }
        ssize_t r = recv(fd, buf, sizeof buf, 0);
        if (r <= 0) break;
        got += (size_t)r;
    }
    double secs = now() - b0;
    printf("tcp ok connect=%.0fms avg_rtt=%.0fms echo256k=%.2fs (%.0f kbps)\n", conn * 1000, rtt * 1000 / n, secs, got * 8 / 1000.0 / secs);
    close(fd);
}
static void udp(const char *ip, int port, int n) {
    struct sockaddr_in a = {.sin_len = sizeof a, .sin_family = AF_INET, .sin_port = htons(port)};
    inet_pton(AF_INET, ip, &a.sin_addr);
    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    struct timeval tv = {1, 0};
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    int replies = 0, errs = 0; double rtt = 0; char b[512] = "x";
    for (int i = 0; i < n; i++) {
        double s0 = now();
        if (sendto(fd, b, 64, 0, (struct sockaddr *)&a, sizeof a) < 0) { errs++; continue; }
        if (recvfrom(fd, b, sizeof b, 0, NULL, NULL) > 0) { replies++; rtt += now() - s0; }
    }
    printf("udp %s sent=%d send_errors=%d replies=%d avg_rtt=%.0fms\n", replies ? "ok" : "err", n, errs, replies, replies ? rtt * 1000 / replies : 0);
    close(fd);
}
static void nw(const char *host, int port, int n, BOOL udpMode) {
    char p[8]; snprintf(p, sizeof p, "%d", port);
    nw_endpoint_t ep = nw_endpoint_create_host(host, p);
    nw_parameters_t params = udpMode ? nw_parameters_create_secure_udp(NW_PARAMETERS_DISABLE_PROTOCOL, NW_PARAMETERS_DEFAULT_CONFIGURATION)
                                     : nw_parameters_create_secure_tcp(NW_PARAMETERS_DISABLE_PROTOCOL, NW_PARAMETERS_DEFAULT_CONFIGURATION);
    nw_connection_t c = nw_connection_create(ep, params);
    dispatch_queue_t q = dispatch_queue_create("nw", NULL);
    dispatch_semaphore_t ready = dispatch_semaphore_create(0);
    __block BOOL ok = NO;
    nw_connection_set_queue(c, q);
    nw_connection_set_state_changed_handler(c, ^(nw_connection_state_t st, nw_error_t e) {
        if (st == nw_connection_state_ready) { ok = YES; dispatch_semaphore_signal(ready); }
        if (st == nw_connection_state_failed || st == nw_connection_state_waiting) dispatch_semaphore_signal(ready);
    });
    double t0 = now();
    nw_connection_start(c);
    const char *kind = udpMode ? "nwudp" : "nwtcp";
    if (dispatch_semaphore_wait(ready, dispatch_time(DISPATCH_TIME_NOW, 15 * NSEC_PER_SEC)) || !ok) { printf("%s err not ready %.2fs\n", kind, now() - t0); nw_connection_cancel(c); return; }
    double conn = now() - t0, rtt = 0; int replies = 0;
    for (int i = 0; i < n; i++) {
        dispatch_semaphore_t got = dispatch_semaphore_create(0);
        dispatch_data_t d = dispatch_data_create("pingpingpingping", 16, q, DISPATCH_DATA_DESTRUCTOR_DEFAULT);
        double s0 = now();
        __block BOOL r = NO;
        nw_connection_send(c, d, NW_CONNECTION_DEFAULT_MESSAGE_CONTEXT, true, ^(nw_error_t e) {});
        nw_connection_receive(c, 1, 65536, ^(dispatch_data_t content, nw_content_context_t ctx, bool complete, nw_error_t e) { r = content != nil; dispatch_semaphore_signal(got); });
        if (dispatch_semaphore_wait(got, dispatch_time(DISPATCH_TIME_NOW, 2 * NSEC_PER_SEC)) == 0 && r) { replies++; rtt += now() - s0; }
    }
    printf("%s %s ready=%.0fms replies=%d/%d avg_rtt=%.0fms\n", kind, replies ? "ok" : "err", conn * 1000, replies, n, replies ? rtt * 1000 / replies : 0);
    nw_connection_cancel(c);
}
static void path(void) {
    nw_path_monitor_t m = nw_path_monitor_create();
    dispatch_semaphore_t sem = dispatch_semaphore_create(0);
    __block int status = -1;
    nw_path_monitor_set_queue(m, dispatch_queue_create("path", NULL));
    nw_path_monitor_set_update_handler(m, ^(nw_path_t p) { status = nw_path_get_status(p); dispatch_semaphore_signal(sem); });
    nw_path_monitor_start(m);
    dispatch_semaphore_wait(sem, dispatch_time(DISPATCH_TIME_NOW, 3 * NSEC_PER_SEC));
    const char *names[] = {"invalid", "satisfied", "unsatisfied", "satisfiable"};
    printf("path %s status=%s\n", status == 1 ? "ok" : "err", status >= 0 && status < 4 ? names[status] : "none");
    nw_path_monitor_cancel(m);
}
static void dns(const char *name) {
    double t0 = now();
    struct addrinfo *res = NULL, hints = {.ai_socktype = SOCK_STREAM};
    int r = getaddrinfo(name, "443", &hints, &res);
    printf("dns %s %s %.0fms\n", r ? "err" : "ok", r ? gai_strerror(r) : "", (now() - t0) * 1000);
    if (res) freeaddrinfo(res);
}
int main(int argc, char **argv) {
    @autoreleasepool {
        if (argc < 3) return 2;
        const char *cmd = argv[1];
        int n = argc > 4 ? atoi(argv[4]) : (argc > 3 ? atoi(argv[3]) : 5);
        if (!strcmp(cmd, "http")) http(argv[2], NO);
        else if (!strcmp(cmd, "h3")) http(argv[2], YES);
        else if (!strcmp(cmd, "ws")) ws(argv[2], argc > 3 ? atoi(argv[3]) : 5);
        else if (!strcmp(cmd, "tcp")) tcp(argv[2], atoi(argv[3]), n);
        else if (!strcmp(cmd, "udp")) udp(argv[2], atoi(argv[3]), n);
        else if (!strcmp(cmd, "nwtcp")) nw(argv[2], atoi(argv[3]), n, NO);
        else if (!strcmp(cmd, "nwudp")) nw(argv[2], atoi(argv[3]), n, YES);
        else if (!strcmp(cmd, "dns")) dns(argv[2]);
        else if (!strcmp(cmd, "path")) path();
    }
    return 0;
}

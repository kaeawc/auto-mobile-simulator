// In-device tool: get or set the device's global proxy in configd_sim via SCDynamicStore (dlsym'd:
// the iOS SDK hides these declarations). Usage: scproxy get | scproxy set <host> <port> | scproxy clear
#import <Foundation/Foundation.h>
#import <dlfcn.h>
typedef const void *SCRef;
int main(int argc, char **argv) {
    @autoreleasepool {
        void *sc = dlopen("/System/Library/Frameworks/SystemConfiguration.framework/SystemConfiguration", RTLD_NOW);
        SCRef (*create)(CFAllocatorRef, CFStringRef, void *, void *) = dlsym(sc, "SCDynamicStoreCreate");
        Boolean (*setv)(SCRef, CFStringRef, CFPropertyListRef) = dlsym(sc, "SCDynamicStoreSetValue");
        CFPropertyListRef (*getv)(SCRef, CFStringRef) = dlsym(sc, "SCDynamicStoreCopyValue");
        Boolean (*rem)(SCRef, CFStringRef) = dlsym(sc, "SCDynamicStoreRemoveValue");
        int (*err)(void) = dlsym(sc, "SCError");
        if (!create || !setv || !getv) { printf("missing SCDynamicStore symbols\n"); return 2; }
        SCRef store = create(NULL, CFSTR("simee"), NULL, NULL);
        CFStringRef key = CFSTR("State:/Network/Global/Proxies");
        const char *cmd = argc > 1 ? argv[1] : "get";
        if (!strcmp(cmd, "set") && argc > 3) {
            NSNumber *port = @(atoi(argv[3]));
            NSString *host = [NSString stringWithUTF8String:argv[2]];
            NSDictionary *d = @{@"HTTPEnable": @1, @"HTTPProxy": host, @"HTTPPort": port,
                                @"HTTPSEnable": @1, @"HTTPSProxy": host, @"HTTPSPort": port,
                                @"SOCKSEnable": @1, @"SOCKSProxy": host, @"SOCKSPort": port,
                                @"ExceptionsList": @[@"localhost", @"127.0.0.1", @"*.local", @"169.254/16"]};
            Boolean ok = setv(store, key, (__bridge CFPropertyListRef)d);
            printf("set %s (SCError %d)\n", ok ? "ok" : "failed", ok ? 0 : err());
        } else if (!strcmp(cmd, "clear")) {
            Boolean ok = rem ? rem(store, key) : 0;
            int code = ok ? 0 : err();
            if (code == 1004) ok = 1; // kSCStatusNoKey: nothing to clear
            printf("clear %s (SCError %d)\n", ok ? "ok" : "failed", code);
        }
        CFPropertyListRef v = getv(store, key);
        printf("%s\n", v ? [[(__bridge id)v description] UTF8String] : "(none)");
        NSDictionary *sys = CFBridgingRelease(CFNetworkCopySystemProxySettings());
        printf("CFNetwork HTTPSEnable=%s HTTPSProxy=%s\n", [[sys[@"HTTPSEnable"] description] UTF8String], [[sys[@"HTTPSProxy"] description] UTF8String]);
    }
    return 0;
}

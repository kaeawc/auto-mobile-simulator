// In-device tool: an app's user defaults through cfprefsd, in the container that holds them (the
// app's data container, an app group's, or none for a domain outside any container), so a running
// app's UserDefaults sees a write at once. The container calls are CoreFoundation SPI, looked up
// with dlsym; without them the tool says so rather than writing the wrong file.
//
//   prefstool read <domain> <container|->   prints "ok <base64 binary plist: every key's value>"
//   prefstool write <request.plist>         takes {domain, container?, set: {key: value},
//                                           delete: [key]}, sets every key, removes every deleted
//                                           one and synchronizes once; prints "ok"
//
// Anything else prints "error <why>" and exits 1.
#import <Foundation/Foundation.h>
#import <dlfcn.h>

typedef CFDictionaryRef (*CopyMultipleFn)(CFArrayRef, CFStringRef, CFStringRef, CFStringRef, CFStringRef);
typedef void (*SetFn)(CFStringRef, CFPropertyListRef, CFStringRef, CFStringRef);
typedef Boolean (*SyncFn)(CFStringRef, CFStringRef);

static int fail(NSString *why) {
    printf("error %s\n", why.UTF8String);
    return 1;
}

static void *spi(const char *name) {
    return dlsym(RTLD_DEFAULT, name);
}

static int readDomain(NSString *domain, NSString *container) {
    NSDictionary *all;
    if (container) {
        CopyMultipleFn copy = (CopyMultipleFn)spi("_CFPreferencesCopyMultipleWithContainer");
        if (!copy) return fail(@"this runtime has no _CFPreferencesCopyMultipleWithContainer");
        all = CFBridgingRelease(copy(NULL, (__bridge CFStringRef)domain, kCFPreferencesCurrentUser, kCFPreferencesAnyHost,
                                     (__bridge CFStringRef)container));
    } else {
        all = CFBridgingRelease(CFPreferencesCopyMultiple(NULL, (__bridge CFStringRef)domain, kCFPreferencesCurrentUser,
                                                          kCFPreferencesAnyHost));
    }
    NSError *error = nil;
    NSData *bytes = [NSPropertyListSerialization dataWithPropertyList:(all ?: @{})
                                                               format:NSPropertyListBinaryFormat_v1_0
                                                              options:0
                                                                error:&error];
    if (!bytes) return fail([NSString stringWithFormat:@"can't encode %@: %@", domain, error.localizedDescription]);
    printf("ok %s\n", [bytes base64EncodedStringWithOptions:0].UTF8String);
    return 0;
}

static int writeDomain(NSString *path) {
    NSData *bytes = [NSData dataWithContentsOfFile:path];
    if (!bytes) return fail([NSString stringWithFormat:@"can't read %@", path]);
    NSError *error = nil;
    NSDictionary *request = [NSPropertyListSerialization propertyListWithData:bytes options:0 format:NULL error:&error];
    NSString *domain = [request isKindOfClass:NSDictionary.class] ? request[@"domain"] : nil;
    if (![domain isKindOfClass:NSString.class]) return fail(@"the request has no domain");
    NSString *container = request[@"container"];
    NSDictionary *set = request[@"set"] ?: @{};
    NSArray *delete = request[@"delete"] ?: @[];
    if (![set isKindOfClass:NSDictionary.class] || ![delete isKindOfClass:NSArray.class]) return fail(@"bad request");
    SetFn setValue = (SetFn)spi("_CFPreferencesSetAppValueWithContainer");
    SyncFn sync = (SyncFn)spi("_CFPreferencesAppSynchronizeWithContainer");
    if (container && (!setValue || !sync)) return fail(@"this runtime has no _CFPreferencesSetAppValueWithContainer");
    CFStringRef d = (__bridge CFStringRef)domain, c = (__bridge CFStringRef)container;
    for (NSString *key in set) {
        if (container) setValue((__bridge CFStringRef)key, (__bridge CFPropertyListRef)set[key], d, c);
        else CFPreferencesSetAppValue((__bridge CFStringRef)key, (__bridge CFPropertyListRef)set[key], d);
    }
    for (NSString *key in delete) {
        if (container) setValue((__bridge CFStringRef)key, NULL, d, c);
        else CFPreferencesSetAppValue((__bridge CFStringRef)key, NULL, d);
    }
    Boolean ok = container ? sync(d, c) : CFPreferencesAppSynchronize(d);
    if (!ok) return fail([NSString stringWithFormat:@"cfprefsd didn't synchronize %@", domain]);
    printf("ok\n");
    return 0;
}

int main(int argc, char **argv) {
    @autoreleasepool {
        if (argc == 4 && !strcmp(argv[1], "read")) {
            NSString *container = strcmp(argv[3], "-") ? @(argv[3]) : nil;
            return readDomain(@(argv[2]), container);
        }
        if (argc == 3 && !strcmp(argv[1], "write")) return writeDomain(@(argv[2]));
        return fail(@"usage: prefstool read <domain> <container|-> | prefstool write <request.plist>");
    }
}

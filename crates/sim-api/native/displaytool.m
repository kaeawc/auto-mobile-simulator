// simee's in-device display tool: asks SpringBoard's display tool service (SBSDisplayToolService,
// what the internal `suiatool display swap` drives) to sweep the iPhone Duo's synthetic hinge to a
// position. SpringBoard moves the content to the other screen when the sweep crosses its swap
// boundary. The simulator has no hinge sensor, so this is how the device folds and unfolds.
//
// Usage: displaytool swap <position 0-1> <seconds>
// Prints "swap ok" once SpringBoard has finished the sweep, or "unavailable: ..." when this runtime
// has no such service. SpringBoard serves only clients with the
// com.apple.springboard.sbdisplay.service entitlement; simee links it into __TEXT,__entitlements,
// where the simulator reads a program's entitlements.
#import <Foundation/Foundation.h>
#import <dlfcn.h>
#import <objc/message.h>
#import <objc/runtime.h>

// -[SBSDisplayToolService swapDisplayWithSettings:completion:]'s settings ({?=qdqqBqdd}, iOS 27.2),
// in the order SpringBoard logs them.
typedef struct {
    long long reserved;
    double duration;             // the visible transition, seconds (0: instant, only from closed)
    long long orientation;       // a UIDeviceOrientation; 0 keeps the current one
    long long profileOverride;   // 0: the selected hinge profile
    BOOL waitForCompletion;      // reply once the sweep has finished
    long long targetMode;        // 1: sweep to targetHingePosition
    double targetHingePosition;  // 0 closed, 1 open flat
    double reserved2;
} SwapSettings;

static const char *kSwap = "swapDisplayWithSettings:completion:";

int main(int argc, char **argv) {
    @autoreleasepool {
        if (argc != 4 || strcmp(argv[1], "swap") != 0) {
            fprintf(stderr, "usage: displaytool swap <position 0-1> <seconds>\n");
            return 64;
        }
        dlopen("/System/Library/PrivateFrameworks/SpringBoardServices.framework/SpringBoardServices", RTLD_NOW);
        Class cls = objc_getClass("SBSDisplayToolService");
        Method method = cls ? class_getInstanceMethod(cls, sel_registerName(kSwap)) : NULL;
        if (!method) {
            printf("unavailable: this runtime has no -[SBSDisplayToolService %s]\n", kSwap);
            return 69;
        }
        const char *types = method_getTypeEncoding(method);
        if (!types || !strstr(types, "{?=qdqqBqdd}")) {
            printf("unavailable: SBSDisplayToolService's %s takes %s\n", kSwap, types ? types : "?");
            return 69;
        }
        SwapSettings settings = {0};
        settings.targetHingePosition = fmin(fmax(atof(argv[2]), 0.0), 1.0);
        settings.duration = fmax(atof(argv[3]), 0.0);
        settings.waitForCompletion = YES;
        settings.targetMode = 1;
        id service = [[cls alloc] init];
        dispatch_semaphore_t done = dispatch_semaphore_create(0);
        ((void (*)(id, SEL, SwapSettings, id))objc_msgSend)(service, sel_registerName(kSwap), settings, ^{
            dispatch_semaphore_signal(done);
        });
        long timedOut = dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, 30 * NSEC_PER_SEC));
        ((void (*)(id, SEL))objc_msgSend)(service, sel_registerName("invalidate"));
        if (timedOut) {
            printf("timeout: SpringBoard didn't answer within 30 s\n");
            return 75;
        }
        printf("swap ok\n");
        return 0;
    }
}

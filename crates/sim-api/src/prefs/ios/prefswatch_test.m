// A host test of prefswatch.m's wrapping (tests.rs builds and runs it on macOS): a class and its
// subclass both implement the change handler, the subclass's calling super's, as CoreFoundation's
// CFPrefsSuiteSearchListSource may do over CFPrefsSearchListSource. Each is wrapped the way the
// observer wraps those two. A notice to the subclass runs each original once and returns (it
// doesn't recurse), and one to a class that inherits the handler runs the same. Prints "ok".
#define main prefswatch_main
#include "prefswatch.m"
#undef main

static int gBaseCalls, gSubCalls;

@interface WatchTestBase : NSObject
- (void)handleChangeNotificationForDomainIdentifier:(CFStringRef)identifier isRemote:(BOOL)remote;
@end
@implementation WatchTestBase
- (void)handleChangeNotificationForDomainIdentifier:(CFStringRef)identifier isRemote:(BOOL)remote {
    gBaseCalls++;
}
@end

@interface WatchTestSub : WatchTestBase
@end
@implementation WatchTestSub
- (void)handleChangeNotificationForDomainIdentifier:(CFStringRef)identifier isRemote:(BOOL)remote {
    gSubCalls++;
    [super handleChangeNotificationForDomainIdentifier:identifier isRemote:remote];
}
@end

@interface WatchTestInherits : WatchTestSub
@end
@implementation WatchTestInherits
@end

static int check(id object, int base, int sub, const char *what) {
    gBaseCalls = gSubCalls = 0;
    [object handleChangeNotificationForDomainIdentifier:CFSTR("d") isRemote:YES];
    if (gBaseCalls == base && gSubCalls == sub) return 0;
    printf("%s: the base handler ran %d times (want %d), the subclass's %d (want %d)\n", what, gBaseCalls, base, gSubCalls, sub);
    return 1;
}

int main(void) {
    @autoreleasepool {
        if (!wrap(@"WatchTestBase") || !wrap(@"WatchTestSub") || !wrap(@"WatchTestInherits")) {
            printf("a test class wasn't wrapped\n");
            return 1;
        }
        int failed = check([WatchTestBase new], 1, 0, "the base class") + check([WatchTestSub new], 1, 1, "the subclass") +
                     check([WatchTestInherits new], 1, 1, "a class that inherits the handler");
        if (failed) return 1;
        printf("ok\n");
        return 0;
    }
}

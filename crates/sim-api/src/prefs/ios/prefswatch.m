// In-device observer for the prefs feed (#235): follows an app's user defaults domains through
// cfprefsd, so a write from any process (the app's own `UserDefaults`, simee's prefstool, another
// tool) reaches the host as it is made instead of when cfprefsd next writes the file (about every
// 10 s on iOS 27.2).
//
//   prefswatch <host pid> <domain> <container> [<domain> <container>]...
//
// prints, for each store i (0-based, in argument order), "snap <i> <base64 binary plist: every
// key's value>" once at start, then "ready", then a new "snap" line each time cfprefsd tells it the
// domain changed and its values differ from the last line for it. A store whose values take more
// than kMaxSnap bytes isn't sent: it prints "large <i>" once instead, until the store fits again
// (the host follows that store's file meanwhile). It waits in the run loop with no timer, so it
// costs nothing until a store changes, and exits when the host process exits or its output is
// closed. Anything that keeps it from following a store prints "error <why>" and exits 1.
//
// cfprefsd tells a process about another process's write to a domain only while the process
// observes the domain with KVO (`NSUserDefaults` then asks cfprefsd to send it changes). The
// observer here watches one key no app uses, and the change notices for every key arrive in
// `-[CFPrefsSearchListSource handleChangeNotificationForDomainIdentifier:isRemote:]` (CoreFoundation
// SPI), which this process wraps in itself. The SPI is looked up and its signature checked first:
// without it the tool says so, and the host keeps following the files.
#import <Foundation/Foundation.h>
#import <dispatch/dispatch.h>
#import <dlfcn.h>
#import <errno.h>
#import <objc/message.h>
#import <objc/runtime.h>
#import <signal.h>
#import <stdlib.h>
#import <string.h>

typedef CFDictionaryRef (*CopyMultipleFn)(CFArrayRef, CFStringRef, CFStringRef, CFStringRef, CFStringRef);
typedef void (*HandleFn)(id, SEL, CFStringRef, BOOL);

/// The key observed to make cfprefsd send changes: never read or written.
static NSString *const kObservedKey = @"dev.simee.prefswatch.observed";
static NSString *const kHandleSelector = @"handleChangeNotificationForDomainIdentifier:isRemote:";
/// The most bytes of a store's binary plist a "snap" line carries (as base64, a third more): the
/// host refuses a line past its own cap (`MAX_LINE` in ios.rs, 1 MiB), which is well above this.
static const NSUInteger kMaxSnap = 256 * 1024;

@interface Store : NSObject
@property NSString *domain;
@property NSString *container;
@property NSUserDefaults *defaults;
@property NSData *last;
@property BOOL pending;
/// Whether the last look found it past kMaxSnap ("large" was printed).
@property BOOL large;
@end
@implementation Store
@end

@interface Observer : NSObject
@end
@implementation Observer
- (void)observeValueForKeyPath:(NSString *)keyPath ofObject:(id)object change:(NSDictionary *)change context:(void *)context {
}
@end

static NSArray<Store *> *gStores;
static CopyMultipleFn gCopy;
/// The handlers wrapped.
static unsigned gWrapped;
/// How deep this thread is in wrapped handlers: a wrapped handler that calls a wrapped one (its
/// superclass's, through `super`) reports the notice once, from the outermost.
static _Thread_local unsigned gDepth;

static int fail(NSString *why) {
    printf("error %s\n", why.UTF8String);
    fflush(stdout);
    return 1;
}

/// Prints store i's values when they differ from the last printed.
static void snap(NSUInteger i) {
    Store *store = gStores[i];
    store.pending = NO;
    NSDictionary *all =
        CFBridgingRelease(gCopy(NULL, (__bridge CFStringRef)store.domain, kCFPreferencesCurrentUser, kCFPreferencesAnyHost,
                                (__bridge CFStringRef)store.container));
    NSData *bytes = [NSPropertyListSerialization dataWithPropertyList:(all ?: @{}) format:NSPropertyListBinaryFormat_v1_0 options:0 error:NULL];
    if (!bytes || [bytes isEqualToData:store.last]) return;
    store.last = bytes;
    if (bytes.length > kMaxSnap) {
        if (!store.large) printf("large %lu\n", (unsigned long)i);
        store.large = YES;
    } else {
        store.large = NO;
        printf("snap %lu %s\n", (unsigned long)i, [bytes base64EncodedStringWithOptions:0].UTF8String);
    }
    fflush(stdout);
}

/// A change notice for `domain`, on the main queue: each store of it is read again once the notices
/// of this turn of the run loop are in (cfprefsd sends more than one for a write). A notice that
/// names no store followed here (or no domain) reads them all: a read whose values haven't changed
/// prints nothing.
static void changed(NSString *domain) {
    BOOL named = NO;
    for (Store *store in gStores) named |= domain && [store.domain isEqualToString:domain];
    for (NSUInteger i = 0; i < gStores.count; i++) {
        Store *store = gStores[i];
        if (store.pending || (named && ![store.domain isEqualToString:domain])) continue;
        store.pending = YES;
        dispatch_async(dispatch_get_main_queue(), ^{
            snap(i);
        });
    }
}

/// Wraps the change handler of `name` (and the classes it is inherited by are covered through it);
/// NO when the class or its method isn't there or its signature isn't the one this was written
/// against (void, a CFString, a BOOL). Each class's wrapper keeps that class's own original: a
/// subclass's handler that calls `super` runs the superclass's wrapper, which runs the superclass's
/// original (never the subclass's again, which would recurse).
static BOOL wrap(NSString *name) {
    Class cls = NSClassFromString(name);
    SEL sel = NSSelectorFromString(kHandleSelector);
    Method m = cls ? class_getInstanceMethod(cls, sel) : NULL;
    if (!m || method_getNumberOfArguments(m) != 4) return NO;
    char ret[8], identifier[32], remote[8];
    method_getReturnType(m, ret, sizeof ret);
    method_getArgumentType(m, 2, identifier, sizeof identifier);
    method_getArgumentType(m, 3, remote, sizeof remote);
    if (strcmp(ret, "v") || strncmp(identifier, "^{__CFString", 12) || (strcmp(remote, "B") && strcmp(remote, "c"))) return NO;
    // Only the class that implements it: a class that inherits it runs the wrapped one.
    unsigned n = 0;
    Method *own = class_copyMethodList(cls, &n);
    BOOL implements = NO;
    for (unsigned i = 0; i < n; i++) implements |= method_getName(own[i]) == sel;
    free(own);
    if (!implements) return YES;
    HandleFn original = (HandleFn)method_getImplementation(m);
    IMP wrapper = imp_implementationWithBlock(^(id self, CFStringRef identifier, BOOL remote) {
        gDepth++;
        original(self, sel, identifier, remote);
        gDepth--;
        if (gDepth) return;
        NSString *domain = identifier ? [(__bridge NSString *)identifier copy] : nil;
        // On the main queue, where the stores are read and printed (a notice comes on cfprefsd's).
        dispatch_async(dispatch_get_main_queue(), ^{
            changed(domain);
        });
    });
    method_setImplementation(m, wrapper);
    gWrapped++;
    return YES;
}

int main(int argc, char **argv) {
    @autoreleasepool {
        if (argc < 4 || argc % 2) return fail(@"usage: prefswatch <host pid> <domain> <container> [<domain> <container>]...");
        pid_t host = (pid_t)atoi(argv[1]);
        if (host <= 0 || (kill(host, 0) != 0 && errno == ESRCH)) return fail(@"the host process isn't running");
        gCopy = (CopyMultipleFn)dlsym(RTLD_DEFAULT, "_CFPreferencesCopyMultipleWithContainer");
        if (!gCopy) return fail(@"this runtime has no _CFPreferencesCopyMultipleWithContainer");
        SEL init = NSSelectorFromString(@"_initWithSuiteName:container:");
        if (![NSUserDefaults instancesRespondToSelector:init]) return fail(@"this runtime's NSUserDefaults has no _initWithSuiteName:container:");
        // The suite source (a defaults object made with a suite name) and the plain one.
        wrap(@"CFPrefsSearchListSource");
        wrap(@"CFPrefsSuiteSearchListSource");
        if (gWrapped == 0) {
            return fail([NSString stringWithFormat:@"this runtime's CoreFoundation has no %@ to follow cfprefsd's changes", kHandleSelector]);
        }

        NSMutableArray<Store *> *stores = [NSMutableArray array];
        Observer *observer = [Observer new];
        for (int i = 2; i + 1 < argc; i += 2) {
            Store *store = [Store new];
            store.domain = @(argv[i]);
            store.container = @(argv[i + 1]);
            NSURL *url = [NSURL fileURLWithPath:store.container isDirectory:YES];
            store.defaults = ((id(*)(id, SEL, id, id))objc_msgSend)([NSUserDefaults alloc], init, store.domain, url);
            if (!store.defaults) return fail([NSString stringWithFormat:@"can't open %@ in %@", store.domain, store.container]);
            [store.defaults addObserver:observer forKeyPath:kObservedKey options:0 context:NULL];
            [stores addObject:store];
        }
        gStores = stores;
        for (NSUInteger i = 0; i < gStores.count; i++) snap(i);
        printf("ready\n");
        fflush(stdout);

        // Ends with the host: its exit, or its end of the output closed (a write then raises SIGPIPE).
        dispatch_source_t exit_watch = dispatch_source_create(DISPATCH_SOURCE_TYPE_PROC, (uintptr_t)host, DISPATCH_PROC_EXIT, dispatch_get_main_queue());
        dispatch_source_set_event_handler(exit_watch, ^{
            exit(0);
        });
        dispatch_resume(exit_watch);
        if (kill(host, 0) != 0 && errno == ESRCH) return 0;
        dispatch_main();
    }
}

// simee's iOS camera shim, stage two (libsimee-camera-hooks.dylib, loaded by loader.c into user
// apps): a back and a front camera for AVFoundation, fed live from simee's camera feed
// (crates/sim-camera/src/ios_feed.rs) over the Unix socket SIMEE_CAMERA_SOCKET names.
//
// Covered: device discovery (AVCaptureDeviceDiscoverySession, defaultDeviceWithMediaType:,
// defaultDeviceWithDeviceType:mediaType:position:, devicesWithMediaType:, deviceWithUniqueID:),
// AVCaptureDeviceInput, AVCaptureSession (inputs, outputs, start/stop with the KVO and
// notifications apps watch), AVCaptureVideoDataOutput sample buffers (BGRA, as the source paces
// them), AVCaptureVideoPreviewLayer content, AVCaptureMetadataOutput QR codes read from the live
// frames, and AVCapturePhotoOutput stills of the latest frame. The camera permission is the
// device's own: grant it with `simee privacy`.
//
// Started from auto-mobile's CameraPosterInjection (a still poster); here every frame comes from
// the feed. Fake objects are created without their class's init and are never freed.
#import <AVFoundation/AVFoundation.h>
#import <CoreGraphics/CoreGraphics.h>
#import <CoreImage/CoreImage.h>
#import <CoreMedia/CoreMedia.h>
#import <CoreVideo/CoreVideo.h>
#import <Foundation/Foundation.h>
#import <UIKit/UIKit.h>
#import <objc/runtime.h>
#include <errno.h>
#include <pthread.h>
#include <stdatomic.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#define SCLog(fmt, ...) NSLog(@"[simee-camera] " fmt, ##__VA_ARGS__)

// ---- Wire format (ios_feed.rs) ----
enum { SCHelloLen = 20, SCHeaderLen = 32, SCMaxSide = 8192 };

static char SCSocketPath[sizeof(((struct sockaddr_un *)0)->sun_path)];
static uint32_t SCWantWidth, SCWantHeight, SCWantFps;

// ---- Fake classes and keys ----
static AVCaptureDevice *SCBack, *SCFront;
static Class SCDiscoveryClass, SCInputClass, SCConnectionClass, SCCodeClass, SCPhotoClass, SCResolvedClass;
static char SCStateKey, SCOutputKey, SCSessionKey, SCTypesKey, SCDeviceKey, SCResolvedKey, SCMessageKey, SCIDKey,
    SCPositionKey, SCSettingsKey, SCPixelsKey, SCImageKey, SCBoundsKey;

// ---- The latest frame and the running sessions (guarded by SCLock) ----
static pthread_mutex_t SCLock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t SCWake = PTHREAD_COND_INITIALIZER;
static CVPixelBufferRef SCLatest;
static NSHashTable<AVCaptureSession *> *SCRunning;
static atomic_bool SCPreviewPending, SCDetecting;
static uint64_t SCLastDetect;

@interface SCOutputState : NSObject
@property(nonatomic, weak) id delegate;
@property(nonatomic, strong) dispatch_queue_t queue;
@property(nonatomic, weak) AVCaptureSession *session;
@property(nonatomic, strong) AVCaptureConnection *connection;
@property(nonatomic) NSInteger inFlight; // samples handed to the delegate's queue, not yet returned
@end
@implementation SCOutputState
@end

@interface SCSessionState : NSObject
@property(nonatomic) BOOL running;
@property(nonatomic) NSUInteger generation;
@property(nonatomic, copy) NSString *preset;
@property(nonatomic, strong) NSMutableArray *inputs;
@property(nonatomic, strong) NSMutableArray<AVCaptureOutput *> *outputs;
@property(nonatomic, strong) NSHashTable<AVCaptureVideoPreviewLayer *> *layers;
@end
@implementation SCSessionState
- (instancetype)init {
    if ((self = [super init])) {
        _inputs = [NSMutableArray array];
        _outputs = [NSMutableArray array];
        _layers = [NSHashTable weakObjectsHashTable];
        _preset = AVCaptureSessionPresetHigh;
    }
    return self;
}
@end

static id SCAssociated(id object, const void *key, Class type) {
    @synchronized(object) {
        id value = objc_getAssociatedObject(object, key);
        if (!value) {
            value = [type new];
            objc_setAssociatedObject(object, key, value, OBJC_ASSOCIATION_RETAIN_NONATOMIC);
        }
        return value;
    }
}
static SCSessionState *SCState(id session) { return SCAssociated(session, &SCStateKey, SCSessionState.class); }
static SCOutputState *SCOutput(id output) { return SCAssociated(output, &SCOutputKey, SCOutputState.class); }
static void SCStore(id object, const void *key, id value) {
    objc_setAssociatedObject(object, key, value, OBJC_ASSOCIATION_RETAIN_NONATOMIC);
}

// class_replaceMethod adds an override when the method is inherited (exchanging an inherited
// Method would change the superclass). A selector this runtime doesn't have is skipped, so an
// older or newer iOS loses one override rather than the app.
static BOOL SCReplace(Class type, SEL selector, id block) {
    Method method = class_getInstanceMethod(type, selector);
    if (!method) {
        SCLog(@"no -[%s %@]; not overridden", class_getName(type), NSStringFromSelector(selector));
        return NO;
    }
    class_replaceMethod(type, selector, imp_implementationWithBlock(block), method_getTypeEncoding(method));
    return YES;
}
static Class SCSubclass(Class parent, const char *name) {
    Class type = objc_allocateClassPair(parent, name, 0);
    objc_registerClassPair(type);
    // Instances come from class_createInstance without the parent's init, so the parent's dealloc
    // would tear down state that was never built: they are leaked instead (a few small objects).
    class_replaceMethod(type, sel_registerName("dealloc"), imp_implementationWithBlock(^(__unsafe_unretained id o){}), "v@:");
    return type;
}
static BOOL SCVideo(AVMediaType media) { return [media isEqualToString:AVMediaTypeVideo]; }
static AVCaptureDevicePosition SCPositionOf(id device) {
    return (AVCaptureDevicePosition)[objc_getAssociatedObject(device, &SCPositionKey) integerValue];
}
static AVCaptureDevice *SCDeviceAt(AVCaptureDevicePosition position) {
    return position == AVCaptureDevicePositionFront ? SCFront : SCBack;
}
static NSArray *SCDevicesAt(AVCaptureDevicePosition position) {
    if (position == AVCaptureDevicePositionUnspecified) return @[ SCBack, SCFront ];
    return @[ SCDeviceAt(position) ];
}

// ---- Frames as CGImages: the pixel buffer itself, locked until the image is freed ----
static void SCReleasePixels(void *info, const void *data, size_t size) {
    CVPixelBufferRef pixels = info;
    CVPixelBufferUnlockBaseAddress(pixels, kCVPixelBufferLock_ReadOnly);
    CVPixelBufferRelease(pixels);
}
static CGImageRef SCCreateImage(CVPixelBufferRef pixels) {
    if (!pixels) return NULL;
    CVPixelBufferRetain(pixels);
    CVPixelBufferLockBaseAddress(pixels, kCVPixelBufferLock_ReadOnly);
    size_t w = CVPixelBufferGetWidth(pixels), h = CVPixelBufferGetHeight(pixels), row = CVPixelBufferGetBytesPerRow(pixels);
    CGDataProviderRef data = CGDataProviderCreateWithData(pixels, CVPixelBufferGetBaseAddress(pixels), row * h, SCReleasePixels);
    CGColorSpaceRef space = CGColorSpaceCreateWithName(kCGColorSpaceSRGB);
    CGImageRef image = CGImageCreate(w, h, 8, 32, row, space, kCGBitmapByteOrder32Little | (CGBitmapInfo)kCGImageAlphaNoneSkipFirst,
                                     data, NULL, false, kCGRenderingIntentDefault);
    CGColorSpaceRelease(space);
    CGDataProviderRelease(data);
    return image;
}
static CVPixelBufferRef SCCopyLatest(void) {
    pthread_mutex_lock(&SCLock);
    CVPixelBufferRef pixels = SCLatest ? CVPixelBufferRetain(SCLatest) : NULL;
    pthread_mutex_unlock(&SCLock);
    return pixels;
}

// ---- Objects: two cameras, discovery, inputs, connections, codes, photos ----
@interface AVCaptureDeviceInput (SimeeCamera)
- (instancetype)sc_initWithDevice:(AVCaptureDevice *)device error:(NSError **)error __attribute__((objc_method_family(init)));
@end
@implementation AVCaptureDeviceInput (SimeeCamera)
- (instancetype)sc_initWithDevice:(AVCaptureDevice *)device error:(NSError **)error {
    if (device != SCBack && device != SCFront) return [self sc_initWithDevice:device error:error];
    self = (id)class_createInstance(SCInputClass, 0);
    if (error) *error = nil;
    SCStore(self, &SCDeviceKey, device);
    return self;
}
@end

static void SCInstallObjects(void) {
    Class device = SCSubclass(AVCaptureDevice.class, "SimeeCameraDevice");
    SCReplace(device, @selector(uniqueID), ^id(id o) {
        return SCPositionOf(o) == AVCaptureDevicePositionFront ? @"simee.camera.front" : @"simee.camera.back";
    });
    SCReplace(device, @selector(localizedName), ^id(id o) {
        return SCPositionOf(o) == AVCaptureDevicePositionFront ? @"simee Front Camera" : @"simee Back Camera";
    });
    SCReplace(device, @selector(modelID), ^id(id o) { return @"simee camera"; });
    SCReplace(device, @selector(manufacturer), ^id(id o) { return @"simee"; });
    SCReplace(device, @selector(position), ^AVCaptureDevicePosition(id o) { return SCPositionOf(o); });
    SCReplace(device, @selector(deviceType), ^id(id o) { return AVCaptureDeviceTypeBuiltInWideAngleCamera; });
    SCReplace(device, @selector(hasMediaType:), ^BOOL(id o, AVMediaType media) { return SCVideo(media); });
    SCReplace(device, @selector(isConnected), ^BOOL(id o) { return YES; });
    SCReplace(device, @selector(isSuspended), ^BOOL(id o) { return NO; });
    SCReplace(device, @selector(formats), ^id(id o) { return @[]; });
    SCReplace(device, @selector(activeFormat), ^id(id o) { return nil; });
    SCReplace(device, @selector(lockForConfiguration:), ^BOOL(id o, NSError **error) { if (error) *error = nil; return YES; });
    SCReplace(device, @selector(unlockForConfiguration), ^(id o){});
    SCReplace(device, @selector(hasTorch), ^BOOL(id o) { return NO; });
    SCReplace(device, @selector(hasFlash), ^BOOL(id o) { return NO; });
    SCReplace(device, @selector(isTorchAvailable), ^BOOL(id o) { return NO; });
    SCReplace(device, @selector(isFocusModeSupported:), ^BOOL(id o, AVCaptureFocusMode m) { return NO; });
    SCReplace(device, @selector(isExposureModeSupported:), ^BOOL(id o, AVCaptureExposureMode m) { return NO; });
    SCReplace(device, @selector(isFocusPointOfInterestSupported), ^BOOL(id o) { return NO; });
    SCReplace(device, @selector(isExposurePointOfInterestSupported), ^BOOL(id o) { return NO; });
    SCReplace(device, @selector(videoZoomFactor), ^CGFloat(id o) { return 1; });
    SCReplace(device, @selector(setVideoZoomFactor:), ^(id o, CGFloat f){});
    SCReplace(device, @selector(setActiveVideoMinFrameDuration:), ^(id o, CMTime t){});
    SCReplace(device, @selector(setActiveVideoMaxFrameDuration:), ^(id o, CMTime t){});
    SCReplace(device, @selector(activeVideoMinFrameDuration), ^CMTime(id o) { return CMTimeMake(1, 30); });
    SCReplace(device, @selector(activeVideoMaxFrameDuration), ^CMTime(id o) { return CMTimeMake(1, 30); });
    SCBack = (id)class_createInstance(device, 0);
    SCStore(SCBack, &SCPositionKey, @(AVCaptureDevicePositionBack));
    SCFront = (id)class_createInstance(device, 0);
    SCStore(SCFront, &SCPositionKey, @(AVCaptureDevicePositionFront));

    SCDiscoveryClass = SCSubclass(AVCaptureDeviceDiscoverySession.class, "SimeeCameraDiscovery");
    SCReplace(SCDiscoveryClass, @selector(devices), ^id(id o) { return SCDevicesAt(SCPositionOf(o)); });
    SCReplace(SCDiscoveryClass, @selector(supportedMultiCamDeviceSets), ^id(id o) { return @[]; });

    SCInputClass = SCSubclass(AVCaptureDeviceInput.class, "SimeeCameraInput");
    SCReplace(SCInputClass, @selector(device), ^id(id o) { return objc_getAssociatedObject(o, &SCDeviceKey); });
    SCReplace(SCInputClass, @selector(ports), ^id(id o) { return @[]; });

    // One connection per output, with the orientation and mirroring setters apps call accepted
    // and ignored: frames arrive upright and unmirrored at the size the feed sends.
    SCConnectionClass = SCSubclass(AVCaptureConnection.class, "SimeeCameraConnection");
    SCReplace(SCConnectionClass, @selector(isEnabled), ^BOOL(id o) { return YES; });
    SCReplace(SCConnectionClass, @selector(setEnabled:), ^(id o, BOOL on){});
    SCReplace(SCConnectionClass, @selector(isActive), ^BOOL(id o) { return YES; });
    SCReplace(SCConnectionClass, @selector(inputPorts), ^id(id o) { return @[]; });
    SCReplace(SCConnectionClass, @selector(output), ^id(id o) { return objc_getAssociatedObject(o, &SCOutputKey); });
    SCReplace(SCConnectionClass, @selector(videoPreviewLayer), ^id(id o) { return nil; });
    SCReplace(SCConnectionClass, @selector(isVideoMirroringSupported), ^BOOL(id o) { return YES; });
    SCReplace(SCConnectionClass, @selector(isVideoMirrored), ^BOOL(id o) { return NO; });
    SCReplace(SCConnectionClass, @selector(setVideoMirrored:), ^(id o, BOOL on){});
    SCReplace(SCConnectionClass, @selector(automaticallyAdjustsVideoMirroring), ^BOOL(id o) { return NO; });
    SCReplace(SCConnectionClass, @selector(setAutomaticallyAdjustsVideoMirroring:), ^(id o, BOOL on){});
    SCReplace(SCConnectionClass, @selector(videoRotationAngle), ^CGFloat(id o) { return 0; });
    SCReplace(SCConnectionClass, @selector(setVideoRotationAngle:), ^(id o, CGFloat a){});
    SCReplace(SCConnectionClass, @selector(isVideoRotationAngleSupported:), ^BOOL(id o, CGFloat a) { return YES; });
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
    SCReplace(SCConnectionClass, @selector(isVideoOrientationSupported), ^BOOL(id o) { return YES; });
    SCReplace(SCConnectionClass, @selector(videoOrientation), ^AVCaptureVideoOrientation(id o) { return AVCaptureVideoOrientationPortrait; });
    SCReplace(SCConnectionClass, @selector(setVideoOrientation:), ^(id o, AVCaptureVideoOrientation v){});
#pragma clang diagnostic pop

    SCCodeClass = SCSubclass(AVMetadataMachineReadableCodeObject.class, "SimeeCameraCode");
    SCReplace(SCCodeClass, @selector(stringValue), ^id(id o) { return objc_getAssociatedObject(o, &SCMessageKey); });
    SCReplace(SCCodeClass, @selector(type), ^id(id o) { return AVMetadataObjectTypeQRCode; });
    SCReplace(SCCodeClass, @selector(bounds), ^CGRect(id o) { return [objc_getAssociatedObject(o, &SCBoundsKey) CGRectValue]; });
    SCReplace(SCCodeClass, @selector(corners), ^id(id o) {
        CGRect b = [objc_getAssociatedObject(o, &SCBoundsKey) CGRectValue];
        return @[
            @{@"X" : @(CGRectGetMinX(b)), @"Y" : @(CGRectGetMinY(b))}, @{@"X" : @(CGRectGetMaxX(b)), @"Y" : @(CGRectGetMinY(b))},
            @{@"X" : @(CGRectGetMaxX(b)), @"Y" : @(CGRectGetMaxY(b))}, @{@"X" : @(CGRectGetMinX(b)), @"Y" : @(CGRectGetMaxY(b))}
        ];
    });
    SCReplace(SCCodeClass, @selector(time), ^CMTime(id o) { return CMClockGetTime(CMClockGetHostTimeClock()); });
    SCReplace(SCCodeClass, @selector(duration), ^CMTime(id o) { return CMTimeMake(1, 2); });

    SCResolvedClass = SCSubclass(AVCaptureResolvedPhotoSettings.class, "SimeeCameraResolvedSettings");
    SCReplace(SCResolvedClass, @selector(uniqueID), ^int64_t(id o) { return [objc_getAssociatedObject(o, &SCIDKey) longLongValue]; });
    SCReplace(SCResolvedClass, @selector(photoDimensions), ^CMVideoDimensions(id o) {
        CVPixelBufferRef p = (__bridge CVPixelBufferRef)objc_getAssociatedObject(o, &SCPixelsKey);
        return p ? (CMVideoDimensions){(int32_t)CVPixelBufferGetWidth(p), (int32_t)CVPixelBufferGetHeight(p)} : (CMVideoDimensions){0, 0};
    });
    SCReplace(SCResolvedClass, @selector(expectedPhotoCount), ^NSInteger(id o) { return 1; });

    SCPhotoClass = SCSubclass(AVCapturePhoto.class, "SimeeCameraPhoto");
    SCReplace(SCPhotoClass, @selector(fileDataRepresentation), ^id(id o) {
        UIImage *image = objc_getAssociatedObject(o, &SCImageKey);
        return image ? UIImageJPEGRepresentation(image, 0.92) : nil;
    });
    SCReplace(SCPhotoClass, @selector(CGImageRepresentation), ^CGImageRef(id o) {
        return ((UIImage *)objc_getAssociatedObject(o, &SCImageKey)).CGImage;
    });
    SCReplace(SCPhotoClass, @selector(pixelBuffer), ^CVPixelBufferRef(id o) {
        return (__bridge CVPixelBufferRef)objc_getAssociatedObject(o, &SCPixelsKey);
    });
    SCReplace(SCPhotoClass, @selector(metadata), ^id(id o) {
        CVPixelBufferRef p = (__bridge CVPixelBufferRef)objc_getAssociatedObject(o, &SCPixelsKey);
        return p ? @{@"PixelWidth" : @(CVPixelBufferGetWidth(p)), @"PixelHeight" : @(CVPixelBufferGetHeight(p))} : @{};
    });
    SCReplace(SCPhotoClass, @selector(resolvedSettings), ^id(id o) { return objc_getAssociatedObject(o, &SCResolvedKey); });
    SCReplace(SCPhotoClass, @selector(photoCount), ^NSInteger(id o) { return 1; });
    SCReplace(SCPhotoClass, @selector(timestamp), ^CMTime(id o) { return CMClockGetTime(CMClockGetHostTimeClock()); });
    SCReplace(SCPhotoClass, @selector(isRawPhoto), ^BOOL(id o) { return NO; });
}

static BOOL SCWantsCamera(NSArray<AVCaptureDeviceType> *types, AVMediaType media) {
    return SCVideo(media) && [types containsObject:AVCaptureDeviceTypeBuiltInWideAngleCamera];
}

static void SCInstallDiscovery(void) {
    Class cls = object_getClass(AVCaptureDevice.class);
    IMP fallback = class_getMethodImplementation(cls, @selector(defaultDeviceWithMediaType:));
    SCReplace(cls, @selector(defaultDeviceWithMediaType:), ^id(id o, AVMediaType media) {
        return SCVideo(media) ? SCBack : ((id(*)(id, SEL, id))fallback)(o, @selector(defaultDeviceWithMediaType:), media);
    });
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
    IMP devices = class_getMethodImplementation(cls, @selector(devicesWithMediaType:));
    SCReplace(cls, @selector(devicesWithMediaType:), ^id(id o, AVMediaType media) {
        return SCVideo(media) ? SCDevicesAt(AVCaptureDevicePositionUnspecified)
                              : ((id(*)(id, SEL, id))devices)(o, @selector(devicesWithMediaType:), media);
    });
#pragma clang diagnostic pop
    IMP typed = class_getMethodImplementation(cls, @selector(defaultDeviceWithDeviceType:mediaType:position:));
    SCReplace(cls, @selector(defaultDeviceWithDeviceType:mediaType:position:),
              ^id(id o, AVCaptureDeviceType type, AVMediaType media, AVCaptureDevicePosition position) {
                  if (SCVideo(media)) return [type isEqual:AVCaptureDeviceTypeBuiltInWideAngleCamera] ? SCDeviceAt(position) : nil;
                  return ((id(*)(id, SEL, id, id, AVCaptureDevicePosition))typed)(
                      o, @selector(defaultDeviceWithDeviceType:mediaType:position:), type, media, position);
              });
    IMP unique = class_getMethodImplementation(cls, @selector(deviceWithUniqueID:));
    SCReplace(cls, @selector(deviceWithUniqueID:), ^id(id o, NSString *uid) {
        for (AVCaptureDevice *d in SCDevicesAt(AVCaptureDevicePositionUnspecified))
            if ([d.uniqueID isEqualToString:uid]) return d;
        return ((id(*)(id, SEL, id))unique)(o, @selector(deviceWithUniqueID:), uid);
    });
    Class discovery = object_getClass(AVCaptureDeviceDiscoverySession.class);
    IMP factory = class_getMethodImplementation(discovery, @selector(discoverySessionWithDeviceTypes:mediaType:position:));
    SCReplace(discovery, @selector(discoverySessionWithDeviceTypes:mediaType:position:),
              ^id(id o, NSArray *types, AVMediaType media, AVCaptureDevicePosition position) {
                  if (SCWantsCamera(types, media)) {
                      id session = class_createInstance(SCDiscoveryClass, 0);
                      SCStore(session, &SCPositionKey, @(position));
                      return session;
                  }
                  return ((id(*)(id, SEL, id, id, AVCaptureDevicePosition))factory)(
                      o, @selector(discoverySessionWithDeviceTypes:mediaType:position:), types, media, position);
              });
    method_exchangeImplementations(class_getInstanceMethod(AVCaptureDeviceInput.class, @selector(initWithDevice:error:)),
                                   class_getInstanceMethod(AVCaptureDeviceInput.class, @selector(sc_initWithDevice:error:)));
    SCReplace(object_getClass(AVCaptureDeviceInput.class), @selector(deviceInputWithDevice:error:),
              ^id(id o, AVCaptureDevice *device, NSError **error) {
                  return [[o alloc] initWithDevice:device error:error]; // the receiver's class, so subclasses keep their behaviour
              });
}

// ---- Delivery: each frame from the feed to every running session ----
static BOOL SCCurrent(SCSessionState *s, NSUInteger generation) {
    @synchronized(s) {
        return s.running && s.generation == generation;
    }
}
static AVCaptureConnection *SCConnectionFor(AVCaptureOutput *output) {
    SCOutputState *binding = SCOutput(output);
    @synchronized(binding) {
        if (!binding.connection) {
            binding.connection = class_createInstance(SCConnectionClass, 0);
            SCStore(binding.connection, &SCOutputKey, output);
        }
        return binding.connection;
    }
}
static NSArray<AVCaptureSession *> *SCRunningSessions(void) {
    pthread_mutex_lock(&SCLock);
    NSArray *sessions = SCRunning.allObjects;
    pthread_mutex_unlock(&SCLock);
    return sessions;
}

static void SCUpdatePreviews(void) {
    if (atomic_exchange(&SCPreviewPending, true)) return; // one main-thread update at a time
    dispatch_async(dispatch_get_main_queue(), ^{
        atomic_store(&SCPreviewPending, false);
        CVPixelBufferRef pixels = SCCopyLatest();
        CGImageRef image = SCCreateImage(pixels);
        if (pixels) CVPixelBufferRelease(pixels);
        if (!image) return;
        for (AVCaptureSession *session in SCRunningSessions()) {
            SCSessionState *s = SCState(session);
            NSArray *layers;
            @synchronized(s) {
                layers = s.layers.allObjects;
            }
            for (AVCaptureVideoPreviewLayer *layer in layers)
                if (layer.session == session) layer.contents = (__bridge id)image;
        }
        CGImageRelease(image);
    });
}

// QR codes in the latest frame, read about twice a second for sessions with a metadata output that
// asked for them, off the feed's thread.
static NSArray *SCReadCodes(CVPixelBufferRef pixels) {
    CIImage *image = [CIImage imageWithCVPixelBuffer:pixels];
    static CIDetector *detector;
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        detector = [CIDetector detectorOfType:CIDetectorTypeQRCode context:nil options:@{CIDetectorAccuracy : CIDetectorAccuracyHigh}];
    });
    CGSize size = image.extent.size;
    NSMutableArray *codes = [NSMutableArray array];
    for (CIQRCodeFeature *feature in [detector featuresInImage:image]) {
        if (!feature.messageString) continue;
        id code = class_createInstance(SCCodeClass, 0);
        SCStore(code, &SCMessageKey, feature.messageString);
        CGRect b = feature.bounds; // Core Image's origin is bottom left; metadata's is top left
        CGRect unit = CGRectMake(b.origin.x / size.width, 1 - CGRectGetMaxY(b) / size.height, b.size.width / size.width,
                                 b.size.height / size.height);
        SCStore(code, &SCBoundsKey, [NSValue valueWithCGRect:unit]);
        [codes addObject:code];
    }
    return codes;
}
static void SCDetect(CVPixelBufferRef pixels, NSArray *targets) {
    uint64_t now = clock_gettime_nsec_np(CLOCK_UPTIME_RAW);
    if (!targets.count || now - SCLastDetect < 500000000ull || atomic_exchange(&SCDetecting, true)) return;
    SCLastDetect = now;
    CVPixelBufferRetain(pixels);
    static dispatch_queue_t queue;
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        queue = dispatch_queue_create("dev.simee.camera.qr", DISPATCH_QUEUE_SERIAL);
    });
    dispatch_async(queue, ^{
        NSArray *codes;
        @autoreleasepool {
            codes = SCReadCodes(pixels);
        }
        CVPixelBufferRelease(pixels);
        atomic_store(&SCDetecting, false);
        if (!codes.count) return;
        for (NSArray *t in targets) {
            AVCaptureMetadataOutput *output = t[0];
            SCOutputState *binding = SCOutput(output);
            id delegate = binding.delegate;
            dispatch_queue_t q = binding.queue;
            if (!delegate || !q) continue;
            dispatch_async(q, ^{
                [delegate captureOutput:output didOutputMetadataObjects:codes fromConnection:SCConnectionFor(output)];
            });
        }
    });
}

static void SCDeliver(CVPixelBufferRef pixels) {
    static CMVideoFormatDescriptionRef format;
    static size_t formatW, formatH;
    size_t w = CVPixelBufferGetWidth(pixels), h = CVPixelBufferGetHeight(pixels);
    if (!format || formatW != w || formatH != h) {
        if (format) CFRelease(format);
        format = NULL;
        if (CMVideoFormatDescriptionCreateForImageBuffer(kCFAllocatorDefault, pixels, &format) != noErr) return;
        formatW = w, formatH = h;
    }
    pthread_mutex_lock(&SCLock);
    CVPixelBufferRef old = SCLatest;
    SCLatest = CVPixelBufferRetain(pixels);
    pthread_mutex_unlock(&SCLock);
    if (old) CVPixelBufferRelease(old);

    CMTime now = CMClockGetTime(CMClockGetHostTimeClock());
    NSMutableArray *qr = [NSMutableArray array];
    BOOL previews = NO;
    for (AVCaptureSession *session in SCRunningSessions()) {
        SCSessionState *s = SCState(session);
        NSArray *outputs;
        NSUInteger generation;
        @synchronized(s) {
            if (!s.running) continue;
            outputs = [s.outputs copy];
            generation = s.generation;
            previews = previews || s.layers.count > 0;
        }
        for (AVCaptureOutput *output in outputs) {
            SCOutputState *binding = SCOutput(output);
            id delegate = binding.delegate;
            dispatch_queue_t queue = binding.queue;
            if (!delegate || !queue) continue;
            if ([output isKindOfClass:AVCaptureVideoDataOutput.class] &&
                [delegate respondsToSelector:@selector(captureOutput:didOutputSampleBuffer:fromConnection:)]) {
                // Like alwaysDiscardsLateVideoFrames: a delegate still on the last frame skips this one.
                @synchronized(binding) {
                    if (binding.inFlight > 0) continue;
                    binding.inFlight++;
                }
                CMSampleTimingInfo timing = {CMTimeMake(1, 30), now, kCMTimeInvalid};
                CMSampleBufferRef sample = NULL;
                if (CMSampleBufferCreateReadyWithImageBuffer(kCFAllocatorDefault, pixels, format, &timing, &sample) != noErr) {
                    @synchronized(binding) {
                        binding.inFlight--;
                    }
                    continue;
                }
                AVCaptureConnection *connection = SCConnectionFor(output);
                dispatch_async(queue, ^{
                    if (SCCurrent(s, generation) && binding.session == session)
                        [delegate captureOutput:(AVCaptureVideoDataOutput *)output didOutputSampleBuffer:sample fromConnection:connection];
                    @synchronized(binding) {
                        binding.inFlight--;
                    }
                    CFRelease(sample);
                });
            } else if ([output isKindOfClass:AVCaptureMetadataOutput.class] &&
                       [delegate respondsToSelector:@selector(captureOutput:didOutputMetadataObjects:fromConnection:)] &&
                       [objc_getAssociatedObject(output, &SCTypesKey) containsObject:AVMetadataObjectTypeQRCode]) {
                [qr addObject:@[ output ]];
            }
        }
    }
    if (previews) SCUpdatePreviews();
    SCDetect(pixels, qr);
}

// ---- The feed reader: one connection per process while any session runs ----
static BOOL SCAnyRunning(void) {
    pthread_mutex_lock(&SCLock);
    BOOL any = SCRunning.count > 0;
    pthread_mutex_unlock(&SCLock);
    return any;
}
typedef enum { SCReadOK, SCReadIdle, SCReadFailed } SCReadResult;
// Reads exactly `len` bytes. A receive timeout before the first byte, with no session running, is
// SCReadIdle (time to disconnect); with sessions running it keeps waiting for the source.
static SCReadResult SCReadFull(int fd, void *buf, size_t len, BOOL frameStart) {
    size_t got = 0;
    while (got < len) {
        ssize_t n = read(fd, (char *)buf + got, len - got);
        if (n > 0) {
            got += (size_t)n;
        } else if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR)) {
            if (frameStart && got == 0 && !SCAnyRunning()) return SCReadIdle;
        } else {
            return SCReadFailed;
        }
    }
    return SCReadOK;
}
static uint32_t SCU32(const uint8_t *b) { return (uint32_t)b[0] | (uint32_t)b[1] << 8 | (uint32_t)b[2] << 16 | (uint32_t)b[3] << 24; }
static uint64_t SCU64(const uint8_t *b) { return (uint64_t)SCU32(b) | (uint64_t)SCU32(b + 4) << 32; }
static void SCPut32(uint8_t *b, uint32_t v) {
    for (int i = 0; i < 4; i++) b[i] = (uint8_t)(v >> (8 * i));
}

static int SCConnect(void) {
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) return -1;
    int on = 1;
    setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &on, sizeof on);
    struct sockaddr_un addr = {.sun_family = AF_UNIX};
    strlcpy(addr.sun_path, SCSocketPath, sizeof addr.sun_path);
    if (connect(fd, (struct sockaddr *)&addr, sizeof addr) != 0) {
        close(fd);
        return -1;
    }
    uint8_t hello[SCHelloLen];
    memcpy(hello, "SIMC", 4);
    SCPut32(hello + 4, 1);
    SCPut32(hello + 8, SCWantWidth);
    SCPut32(hello + 12, SCWantHeight);
    SCPut32(hello + 16, SCWantFps);
    if (write(fd, hello, sizeof hello) != sizeof hello) {
        close(fd);
        return -1;
    }
    struct timeval timeout = {.tv_sec = 0, .tv_usec = 500000};
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof timeout);
    return fd;
}

typedef enum { SCEndedAfterFrames, SCEndedWithoutFrames, SCEndedIdle, SCEndedProtocol } SCEnd;

// One connection's frames, until the feed goes away or no session runs.
static SCEnd SCReadFrames(int fd) {
    CVPixelBufferPoolRef pool = NULL;
    size_t poolW = 0, poolH = 0;
    BOOL logged = NO;
    SCEnd end = SCEndedWithoutFrames;
    for (;;) {
        // Checked at every frame, not only on a receive timeout: a live source never lets one fire.
        if (!SCAnyRunning()) {
            end = SCEndedIdle;
            break;
        }
        uint8_t head[SCHeaderLen];
        if (SCReadFull(fd, head, sizeof head, YES) != SCReadOK) break;
        if (memcmp(head, "SIMF", 4) != 0) {
            // Not a frame header: a feed of another protocol version (it refuses version 1 by closing,
            // so a mismatch seen here is a feed that answers with something else).
            end = SCEndedProtocol;
            break;
        }
        uint32_t w = SCU32(head + 4), h = SCU32(head + 8), generation = SCU32(head + 12);
        uint64_t seq = SCU64(head + 16);
        if (w < 32 || h < 32 || w > SCMaxSide || h > SCMaxSide) break;
        if (!pool || poolW != w || poolH != h) {
            if (pool) CVPixelBufferPoolRelease(pool);
            NSDictionary *attrs = @{
                (id)kCVPixelBufferPixelFormatTypeKey : @(kCVPixelFormatType_32BGRA),
                (id)kCVPixelBufferWidthKey : @(w),
                (id)kCVPixelBufferHeightKey : @(h),
                (id)kCVPixelBufferIOSurfacePropertiesKey : @{},
                (id)kCVPixelBufferCGImageCompatibilityKey : @YES,
            };
            pool = NULL;
            if (CVPixelBufferPoolCreate(kCFAllocatorDefault, NULL, (__bridge CFDictionaryRef)attrs, &pool) != kCVReturnSuccess) break;
            poolW = w, poolH = h;
        }
        CVPixelBufferRef pixels = NULL;
        if (CVPixelBufferPoolCreatePixelBuffer(kCFAllocatorDefault, pool, &pixels) != kCVReturnSuccess) break;
        // Straight into the (IOSurface-backed) buffer, a row at a time when its rows are padded.
        CVPixelBufferLockBaseAddress(pixels, 0);
        uint8_t *base = CVPixelBufferGetBaseAddress(pixels);
        size_t row = CVPixelBufferGetBytesPerRow(pixels), packed = (size_t)w * 4;
        SCReadResult read = SCReadOK;
        if (row == packed) {
            read = SCReadFull(fd, base, packed * h, NO);
        } else {
            for (uint32_t y = 0; y < h && read == SCReadOK; y++) read = SCReadFull(fd, base + y * row, packed, NO);
        }
        CVPixelBufferUnlockBaseAddress(pixels, 0);
        if (read != SCReadOK) {
            CVPixelBufferRelease(pixels);
            break;
        }
        CVBufferSetAttachment(pixels, CFSTR("SimeeCameraSeq"), (__bridge CFTypeRef) @(seq), kCVAttachmentMode_ShouldNotPropagate);
        CVBufferSetAttachment(pixels, CFSTR("SimeeCameraGeneration"), (__bridge CFTypeRef) @(generation),
                              kCVAttachmentMode_ShouldNotPropagate);
        if (!logged) {
            SCLog(@"first frame %ux%u seq %llu generation %u", w, h, seq, generation);
            logged = YES;
        }
        end = SCEndedAfterFrames;
        @autoreleasepool {
            SCDeliver(pixels);
        }
        CVPixelBufferRelease(pixels);
    }
    if (pool) CVPixelBufferPoolRelease(pool);
    return end;
}

static void *SCReaderThread(void *arg) {
    pthread_setname_np("simee camera feed");
    // Backoff between connections that delivered nothing (feed down, refused, broken): 250 ms,
    // doubling to 5 s; a connection that delivered frames, or an idle disconnect, starts over.
    useconds_t backoff = 250000;
    for (;;) {
        pthread_mutex_lock(&SCLock);
        while (SCRunning.count == 0) pthread_cond_wait(&SCWake, &SCLock);
        pthread_mutex_unlock(&SCLock);
        int fd = SCConnect();
        SCEnd end = SCEndedWithoutFrames;
        if (fd >= 0) {
            @autoreleasepool {
                end = SCReadFrames(fd);
            }
            close(fd);
        }
        if (end == SCEndedProtocol) {
            // The feed speaks another protocol: retrying can't help. Stay down for this process.
            SCLog(@"the feed's reply is not this shim's protocol (version 1): not reconnecting");
            for (;;) pause();
        }
        if (end == SCEndedAfterFrames || end == SCEndedIdle) {
            backoff = 250000;
            if (end == SCEndedAfterFrames) usleep(backoff); // the feed went away: don't spin on a restart
            continue;
        }
        usleep(backoff);
        if (backoff < 5000000) backoff = backoff * 2 > 5000000 ? 5000000 : backoff * 2;
    }
    return NULL;
}

// ---- Sessions, preview layers and outputs ----
static void SCBindLayer(AVCaptureVideoPreviewLayer *layer, AVCaptureSession *session) {
    AVCaptureSession *old = objc_getAssociatedObject(layer, &SCSessionKey);
    if (old) {
        SCSessionState *s = SCState(old);
        @synchronized(s) {
            [s.layers removeObject:layer];
        }
    }
    SCStore(layer, &SCSessionKey, session);
    if (session) {
        SCSessionState *s = SCState(session);
        @synchronized(s) {
            [s.layers addObject:layer];
        }
    }
    layer.contents = nil;
    if (session.isRunning) SCUpdatePreviews();
}
@interface AVCaptureVideoPreviewLayer (SimeeCamera)
- (instancetype)sc_initWithSession:(AVCaptureSession *)session __attribute__((objc_method_family(init)));
@end
@implementation AVCaptureVideoPreviewLayer (SimeeCamera)
- (instancetype)sc_initWithSession:(AVCaptureSession *)session {
    // After the exchange this is the stock initializer: run it with no session so the layer's own
    // state is set up, then bind the session here.
    self = [self sc_initWithSession:nil];
    if (self) {
        self.videoGravity = AVLayerVideoGravityResizeAspect;
        SCBindLayer(self, session);
    }
    return self;
}
@end

static void SCSetRunning(AVCaptureSession *session, BOOL running) {
    SCSessionState *s = SCState(session);
    NSArray *layers;
    [session willChangeValueForKey:@"running"];
    @synchronized(s) {
        if (s.running == running) {
            [session didChangeValueForKey:@"running"];
            return;
        }
        s.running = running;
        s.generation++;
        layers = s.layers.allObjects;
    }
    pthread_mutex_lock(&SCLock);
    if (running) [SCRunning addObject:session];
    else [SCRunning removeObject:session];
    pthread_cond_broadcast(&SCWake);
    pthread_mutex_unlock(&SCLock);
    [session didChangeValueForKey:@"running"];
    NSNotificationName name = running ? AVCaptureSessionDidStartRunningNotification : AVCaptureSessionDidStopRunningNotification;
    [NSNotificationCenter.defaultCenter postNotificationName:name object:session];
    if (!running) {
        dispatch_async(dispatch_get_main_queue(), ^{
            for (AVCaptureVideoPreviewLayer *layer in layers)
                if (layer.session == session && !session.isRunning) layer.contents = nil;
        });
    }
}

static void SCInstallSession(void) {
    Class cls = AVCaptureSession.class;
    SCReplace(cls, @selector(canAddInput:), ^BOOL(id o, AVCaptureInput *input) { return input != nil; });
    SCReplace(cls, @selector(addInput:), ^(id o, AVCaptureInput *input) {
        SCSessionState *s = SCState(o);
        @synchronized(s) {
            if (input && ![s.inputs containsObject:input]) [s.inputs addObject:input];
        }
    });
    SCReplace(cls, @selector(removeInput:), ^(id o, AVCaptureInput *input) {
        SCSessionState *s = SCState(o);
        @synchronized(s) {
            [s.inputs removeObject:input];
        }
    });
    SCReplace(cls, @selector(inputs), ^id(id o) {
        SCSessionState *s = SCState(o);
        @synchronized(s) {
            return [s.inputs copy];
        }
    });
    SCReplace(cls, @selector(canAddOutput:), ^BOOL(id o, AVCaptureOutput *output) { return output != nil; });
    SCReplace(cls, @selector(addOutput:), ^(AVCaptureSession *o, AVCaptureOutput *output) {
        SCSessionState *s = SCState(o);
        @synchronized(s) {
            if (output && ![s.outputs containsObject:output]) {
                [s.outputs addObject:output];
                SCOutput(output).session = o;
            }
        }
    });
    SCReplace(cls, @selector(removeOutput:), ^(id o, AVCaptureOutput *output) {
        SCSessionState *s = SCState(o);
        @synchronized(s) {
            [s.outputs removeObject:output];
            SCOutput(output).session = nil;
        }
    });
    SCReplace(cls, @selector(outputs), ^id(id o) {
        SCSessionState *s = SCState(o);
        @synchronized(s) {
            return [s.outputs copy];
        }
    });
    SCReplace(cls, @selector(connections), ^id(id o) {
        NSMutableArray *connections = [NSMutableArray array];
        for (AVCaptureOutput *output in [o outputs]) [connections addObject:SCConnectionFor(output)];
        return connections;
    });
    SCReplace(cls, @selector(beginConfiguration), ^(id o){});
    SCReplace(cls, @selector(commitConfiguration), ^(id o){});
    SCReplace(cls, @selector(setSessionPreset:), ^(id o, NSString *preset) {
        SCSessionState *s = SCState(o);
        @synchronized(s) {
            s.preset = preset;
        }
    });
    SCReplace(cls, @selector(sessionPreset), ^id(id o) {
        SCSessionState *s = SCState(o);
        @synchronized(s) {
            return s.preset;
        }
    });
    SCReplace(cls, @selector(canSetSessionPreset:), ^BOOL(id o, NSString *preset) { return YES; });
    SCReplace(cls, @selector(isRunning), ^BOOL(id o) {
        SCSessionState *s = SCState(o);
        @synchronized(s) {
            return s.running;
        }
    });
    SCReplace(cls, @selector(isInterrupted), ^BOOL(id o) { return NO; });
    SCReplace(cls, @selector(startRunning), ^(AVCaptureSession *o) { SCSetRunning(o, YES); });
    SCReplace(cls, @selector(stopRunning), ^(AVCaptureSession *o) { SCSetRunning(o, NO); });
}

static void SCInstallPreviewAndOutputs(void) {
    Class layer = AVCaptureVideoPreviewLayer.class;
    method_exchangeImplementations(class_getInstanceMethod(layer, @selector(initWithSession:)),
                                   class_getInstanceMethod(layer, @selector(sc_initWithSession:)));
    SCReplace(object_getClass(layer), @selector(layerWithSession:),
              ^id(id o, AVCaptureSession *s) { return [[o alloc] initWithSession:s]; });
    SCReplace(layer, @selector(setSession:), ^(AVCaptureVideoPreviewLayer *o, AVCaptureSession *s) { SCBindLayer(o, s); });
    SCReplace(layer, @selector(session), ^id(id o) { return objc_getAssociatedObject(o, &SCSessionKey); });
    SCReplace(layer, @selector(connection), ^id(id o) { return nil; });
    SCReplace(layer, @selector(setVideoGravity:), ^(AVCaptureVideoPreviewLayer *o, AVLayerVideoGravity gravity) {
        o.contentsGravity = [gravity isEqual:AVLayerVideoGravityResizeAspectFill]
                                ? kCAGravityResizeAspectFill
                                : ([gravity isEqual:AVLayerVideoGravityResize] ? kCAGravityResize : kCAGravityResizeAspect);
    });
    SCReplace(layer, @selector(videoGravity), ^id(AVCaptureVideoPreviewLayer *o) {
        return [o.contentsGravity isEqual:kCAGravityResizeAspectFill]
                   ? AVLayerVideoGravityResizeAspectFill
                   : ([o.contentsGravity isEqual:kCAGravityResize] ? AVLayerVideoGravityResize : AVLayerVideoGravityResizeAspect);
    });

    Class video = AVCaptureVideoDataOutput.class;
    SCReplace(video, @selector(setSampleBufferDelegate:queue:), ^(id o, id delegate, dispatch_queue_t queue) {
        SCOutputState *s = SCOutput(o);
        @synchronized(s) {
            s.delegate = delegate;
            s.queue = queue ?: dispatch_get_main_queue();
        }
    });
    SCReplace(video, @selector(sampleBufferDelegate), ^id(id o) { return SCOutput(o).delegate; });
    SCReplace(video, @selector(sampleBufferCallbackQueue), ^id(id o) { return SCOutput(o).queue; });
    SCReplace(video, @selector(availableVideoCVPixelFormatTypes), ^id(id o) { return @[ @(kCVPixelFormatType_32BGRA) ]; });
    SCReplace(video, @selector(setVideoSettings:), ^(id o, NSDictionary *settings) { SCStore(o, &SCSettingsKey, [settings copy]); });
    SCReplace(video, @selector(videoSettings), ^id(id o) {
        return objc_getAssociatedObject(o, &SCSettingsKey) ?: @{(id)kCVPixelBufferPixelFormatTypeKey : @(kCVPixelFormatType_32BGRA)};
    });
    for (Class output in @[ AVCaptureVideoDataOutput.class, AVCaptureMetadataOutput.class, AVCapturePhotoOutput.class ]) {
        SCReplace(output, @selector(connectionWithMediaType:), ^id(id o, AVMediaType type) { return SCConnectionFor(o); });
        SCReplace(output, @selector(connections), ^id(id o) { return @[ SCConnectionFor(o) ]; });
    }

    Class metadata = AVCaptureMetadataOutput.class;
    SCReplace(metadata, @selector(setMetadataObjectsDelegate:queue:), ^(id o, id delegate, dispatch_queue_t queue) {
        SCOutputState *s = SCOutput(o);
        @synchronized(s) {
            s.delegate = delegate;
            s.queue = queue ?: dispatch_get_main_queue();
        }
    });
    SCReplace(metadata, @selector(setMetadataObjectTypes:), ^(id o, NSArray *types) { SCStore(o, &SCTypesKey, [types copy]); });
    SCReplace(metadata, @selector(metadataObjectTypes), ^id(id o) { return objc_getAssociatedObject(o, &SCTypesKey) ?: @[]; });
    SCReplace(metadata, @selector(availableMetadataObjectTypes), ^id(id o) { return @[ AVMetadataObjectTypeQRCode ]; });

    SCReplace(AVCapturePhotoOutput.class, @selector(capturePhotoWithSettings:delegate:),
              ^(AVCapturePhotoOutput *o, AVCapturePhotoSettings *settings, id<AVCapturePhotoCaptureDelegate> delegate) {
                  CVPixelBufferRef pixels = SCCopyLatest();
                  id resolved = class_createInstance(SCResolvedClass, 0);
                  SCStore(resolved, &SCIDKey, @(settings.uniqueID));
                  id photo = class_createInstance(SCPhotoClass, 0);
                  SCStore(photo, &SCResolvedKey, resolved);
                  if (pixels) {
                      SCStore(resolved, &SCPixelsKey, (__bridge id)pixels);
                      SCStore(photo, &SCPixelsKey, (__bridge id)pixels);
                      CGImageRef image = SCCreateImage(pixels);
                      if (image) SCStore(photo, &SCImageKey, [UIImage imageWithCGImage:image]);
                      CGImageRelease(image);
                      CVPixelBufferRelease(pixels);
                  }
                  AVCaptureSession *session = SCOutput(o).session;
                  NSError *error = !session.isRunning ? [NSError errorWithDomain:@"SimeeCamera" code:1
                                                                        userInfo:@{NSLocalizedDescriptionKey : @"The capture session is not running"}]
                                   : !pixels      ? [NSError errorWithDomain:@"SimeeCamera" code:2
                                                                   userInfo:@{NSLocalizedDescriptionKey : @"No frame from the simee camera feed yet"}]
                                                  : nil;
                  dispatch_async(dispatch_get_main_queue(), ^{
                      if ([delegate respondsToSelector:@selector(captureOutput:willBeginCaptureForResolvedSettings:)])
                          [delegate captureOutput:o willBeginCaptureForResolvedSettings:resolved];
                      if ([delegate respondsToSelector:@selector(captureOutput:didFinishProcessingPhoto:error:)])
                          [delegate captureOutput:o didFinishProcessingPhoto:photo error:error];
                      if ([delegate respondsToSelector:@selector(captureOutput:didFinishCaptureForResolvedSettings:error:)])
                          [delegate captureOutput:o didFinishCaptureForResolvedSettings:resolved error:error];
                  });
              });
}

// "WIDTHxHEIGHT@FPS" (each part optional as 0: the feed's default).
static void SCParseSize(const char *text) {
    unsigned w = 0, h = 0, fps = 0;
    if (text && sscanf(text, "%ux%u@%u", &w, &h, &fps) >= 2) SCWantWidth = w, SCWantHeight = h, SCWantFps = fps;
}

__attribute__((constructor)) static void SCActivate(void) {
    @autoreleasepool {
        const char *socket = getenv("SIMEE_CAMERA_SOCKET");
        if (!socket || !*socket) return;
        NSString *bundle = NSBundle.mainBundle.bundleIdentifier;
        if (!bundle || [bundle hasPrefix:@"com.apple."]) return;
        if (strlen(socket) >= sizeof SCSocketPath) {
            SCLog(@"%s is too long for a Unix socket path; no camera", socket);
            return;
        }
        strlcpy(SCSocketPath, socket, sizeof SCSocketPath);
        SCParseSize(getenv("SIMEE_CAMERA_SIZE"));
        SCRunning = [NSHashTable weakObjectsHashTable];
        SCInstallObjects();
        SCInstallDiscovery();
        SCInstallSession();
        SCInstallPreviewAndOutputs();
        pthread_t thread;
        pthread_attr_t attr;
        pthread_attr_init(&attr);
        pthread_attr_setdetachstate(&attr, PTHREAD_CREATE_DETACHED);
        if (pthread_create(&thread, &attr, SCReaderThread, NULL) != 0) SCLog(@"can't start the feed thread");
        pthread_attr_destroy(&attr);
        SCLog(@"active in %@: back and front cameras from %s", bundle, socket);
    }
}

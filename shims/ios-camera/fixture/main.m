// CameraFixture: a plain AVFoundation camera app for checking simee's camera shim. It opens the
// back camera (or the front with launch arguments `-position front`), shows the preview full
// screen, and reports over it, and in Documents/camera-report.json, what the frames carry: the
// test card's frame number read back from the pixels of each sample buffer, the feed's own number
// and link generation (attachments the shim sets), the frame rate, and QR codes from a metadata
// output.
#import <AVFoundation/AVFoundation.h>
#import <QuartzCore/QuartzCore.h>
#import <UIKit/UIKit.h>

@interface FixtureController : UIViewController <AVCaptureVideoDataOutputSampleBufferDelegate, AVCaptureMetadataOutputObjectsDelegate>
@end

@implementation FixtureController {
    AVCaptureSession *_session;
    AVCaptureVideoPreviewLayer *_preview;
    UILabel *_status;
    dispatch_queue_t _frames;
    NSString *_position, *_deviceName, *_authorization, *_error;
    NSUInteger _discovered;
    // Written on the frames queue, read on main under @synchronized(self).
    uint64_t _count, _seq, _stripSeq, _generation;
    size_t _width, _height;
    double _fps;
    NSMutableArray<NSNumber *> *_times;
    NSString *_qr;
}

// The test card's frame number from its top strip: 32 cells, most significant bit first, white
// for 1 (crates/sim-camera's synth::testcard_seq).
static uint32_t StripSeq(CVPixelBufferRef pixels) {
    size_t w = CVPixelBufferGetWidth(pixels), h = CVPixelBufferGetHeight(pixels), row = CVPixelBufferGetBytesPerRow(pixels);
    size_t cell = w / 32, strip = h / 16 > 2 ? h / 16 : 2;
    if (!cell) return 0;
    const uint8_t *base = CVPixelBufferGetBaseAddress(pixels);
    uint32_t seq = 0;
    for (size_t bit = 0; bit < 32; bit++) {
        const uint8_t *px = base + (strip / 2) * row + (bit * cell + cell / 2) * 4;
        seq = seq << 1 | ((px[0] + px[1] + px[2]) / 3 > 127);
    }
    return seq;
}

- (void)viewDidLoad {
    [super viewDidLoad];
    self.view.backgroundColor = UIColor.blackColor;
    _times = [NSMutableArray array];
    _position = [[NSUserDefaults.standardUserDefaults stringForKey:@"position"] isEqualToString:@"front"] ? @"front" : @"back";
    _session = [AVCaptureSession new];
    _preview = [AVCaptureVideoPreviewLayer layerWithSession:_session];
    _preview.videoGravity = AVLayerVideoGravityResizeAspectFill;
    [self.view.layer addSublayer:_preview];
    _status = [UILabel new];
    _status.numberOfLines = 0;
    _status.textColor = UIColor.whiteColor;
    _status.backgroundColor = [UIColor colorWithWhite:0 alpha:0.72];
    _status.font = [UIFont monospacedSystemFontOfSize:11 weight:UIFontWeightSemibold];
    [self.view addSubview:_status];
    _frames = dispatch_queue_create("dev.simee.camerafixture.frames", DISPATCH_QUEUE_SERIAL);
    [AVCaptureDevice requestAccessForMediaType:AVMediaTypeVideo completionHandler:^(BOOL granted) {
        dispatch_async(dispatch_get_main_queue(), ^{
            [self start];
        });
    }];
    [NSTimer scheduledTimerWithTimeInterval:0.25 repeats:YES block:^(NSTimer *t) {
        [self report];
    }];
}

- (void)viewDidLayoutSubviews {
    [super viewDidLayoutSubviews];
    _preview.frame = self.view.bounds;
    CGRect b = self.view.bounds;
    // Kept inside the bottom fifth of the screen, below the camera bench's centre box (the middle 60%
    // of each axis, where its --point must fall), so the label never darkens a pixel the bench reads.
    CGFloat top = b.size.height * 0.8 + 2;
    _status.frame = CGRectMake(12, top, b.size.width - 24, b.size.height - self.view.safeAreaInsets.bottom - top - 2);
    _status.clipsToBounds = YES;
}

- (void)start {
    switch ([AVCaptureDevice authorizationStatusForMediaType:AVMediaTypeVideo]) {
    case AVAuthorizationStatusAuthorized: _authorization = @"authorized"; break;
    case AVAuthorizationStatusDenied: _authorization = @"denied"; break;
    case AVAuthorizationStatusRestricted: _authorization = @"restricted"; break;
    default: _authorization = @"not-determined"; break;
    }
    AVCaptureDevicePosition position = [_position isEqualToString:@"front"] ? AVCaptureDevicePositionFront : AVCaptureDevicePositionBack;
    AVCaptureDeviceDiscoverySession *discovery =
        [AVCaptureDeviceDiscoverySession discoverySessionWithDeviceTypes:@[ AVCaptureDeviceTypeBuiltInWideAngleCamera ]
                                                               mediaType:AVMediaTypeVideo
                                                                position:AVCaptureDevicePositionUnspecified];
    _discovered = discovery.devices.count;
    AVCaptureDevice *device = [AVCaptureDevice defaultDeviceWithDeviceType:AVCaptureDeviceTypeBuiltInWideAngleCamera
                                                                 mediaType:AVMediaTypeVideo
                                                                  position:position];
    if (!device) {
        _error = @"no camera";
        return;
    }
    _deviceName = device.localizedName;
    NSError *error;
    AVCaptureDeviceInput *input = [AVCaptureDeviceInput deviceInputWithDevice:device error:&error];
    if (!input || ![_session canAddInput:input]) {
        _error = error.localizedDescription ?: @"can't add the camera input";
        return;
    }
    [_session beginConfiguration];
    [_session addInput:input];
    AVCaptureVideoDataOutput *video = [AVCaptureVideoDataOutput new];
    video.videoSettings = @{(id)kCVPixelBufferPixelFormatTypeKey : @(kCVPixelFormatType_32BGRA)};
    [video setSampleBufferDelegate:self queue:_frames];
    if ([_session canAddOutput:video]) [_session addOutput:video];
    AVCaptureMetadataOutput *metadata = [AVCaptureMetadataOutput new];
    if ([_session canAddOutput:metadata]) {
        [_session addOutput:metadata];
        [metadata setMetadataObjectsDelegate:self queue:dispatch_get_main_queue()];
        if ([metadata.availableMetadataObjectTypes containsObject:AVMetadataObjectTypeQRCode])
            metadata.metadataObjectTypes = @[ AVMetadataObjectTypeQRCode ];
    }
    [_session commitConfiguration];
    AVCaptureSession *session = _session;
    dispatch_async(_frames, ^{
        [session startRunning];
    });
}

- (void)captureOutput:(AVCaptureOutput *)output didOutputSampleBuffer:(CMSampleBufferRef)sample fromConnection:(AVCaptureConnection *)connection {
    CVPixelBufferRef pixels = CMSampleBufferGetImageBuffer(sample);
    if (!pixels) return;
    CVPixelBufferLockBaseAddress(pixels, kCVPixelBufferLock_ReadOnly);
    uint32_t strip = StripSeq(pixels);
    CVPixelBufferUnlockBaseAddress(pixels, kCVPixelBufferLock_ReadOnly);
    NSNumber *seq = (__bridge_transfer NSNumber *)CVBufferCopyAttachment(pixels, CFSTR("SimeeCameraSeq"), NULL);
    NSNumber *generation = (__bridge_transfer NSNumber *)CVBufferCopyAttachment(pixels, CFSTR("SimeeCameraGeneration"), NULL);
    double now = CACurrentMediaTime();
    @synchronized(self) {
        _count++;
        _stripSeq = strip;
        _seq = seq.unsignedLongLongValue;
        _generation = generation.unsignedLongLongValue;
        _width = CVPixelBufferGetWidth(pixels);
        _height = CVPixelBufferGetHeight(pixels);
        [_times addObject:@(now)];
        while (_times.count && now - _times.firstObject.doubleValue > 1) [_times removeObjectAtIndex:0];
        _fps = _times.count;
    }
}

- (void)captureOutput:(AVCaptureOutput *)output didOutputMetadataObjects:(NSArray<AVMetadataObject *> *)objects fromConnection:(AVCaptureConnection *)connection {
    for (AVMetadataObject *object in objects) {
        if ([object isKindOfClass:AVMetadataMachineReadableCodeObject.class]) {
            NSString *text = ((AVMetadataMachineReadableCodeObject *)object).stringValue;
            if (text) @synchronized(self) {
                    _qr = text;
                }
        }
    }
}

- (void)report {
    NSDictionary *r;
    @synchronized(self) {
        // A QR frame's top strip is the white quiet zone: all ones, not a test card number.
        BOOL testcard = _count && _stripSeq != UINT32_MAX;
        r = @{
            @"position" : _position,
            @"device" : _deviceName ?: [NSNull null],
            @"authorization" : _authorization ?: [NSNull null],
            @"discovered" : @(_discovered),
            @"running" : @(_session.isRunning),
            @"frames" : @(_count),
            @"fps" : @(_fps),
            @"width" : @(_width),
            @"height" : @(_height),
            @"testcard_seq" : testcard ? @(_stripSeq) : [NSNull null],
            @"feed_seq" : @(_seq),
            @"generation" : @(_generation),
            @"qr" : _qr ?: [NSNull null],
            @"error" : _error ?: [NSNull null],
        };
    }
    NSString *testcard = r[@"testcard_seq"] == [NSNull null] ? @"-" : [r[@"testcard_seq"] stringValue];
    _status.text = [NSString stringWithFormat:@" CameraFixture  %@ camera\n %@\n frames %@  %@ fps  %@x%@\n testcard #%@\n feed seq %@  gen %@\n QR: %@%@",
                                              _position, r[@"device"] == [NSNull null] ? @"(no device)" : r[@"device"], r[@"frames"],
                                              r[@"fps"], r[@"width"], r[@"height"], testcard, r[@"feed_seq"], r[@"generation"],
                                              _qr ?: @"-", _error ? [@"\n error: " stringByAppendingString:_error] : @""];
    NSURL *docs = [NSFileManager.defaultManager URLsForDirectory:NSDocumentDirectory inDomains:NSUserDomainMask].firstObject;
    NSData *json = [NSJSONSerialization dataWithJSONObject:r options:NSJSONWritingSortedKeys error:nil];
    [json writeToURL:[docs URLByAppendingPathComponent:@"camera-report.json"] atomically:YES];
}
@end

@interface FixtureDelegate : UIResponder <UIApplicationDelegate>
@end

@implementation FixtureDelegate
@end

@interface FixtureSceneDelegate : UIResponder <UIWindowSceneDelegate>
@property(nonatomic, strong) UIWindow *window;
@end

@implementation FixtureSceneDelegate
- (void)scene:(UIScene *)scene willConnectToSession:(UISceneSession *)session options:(UISceneConnectionOptions *)options {
    self.window = [[UIWindow alloc] initWithWindowScene:(UIWindowScene *)scene];
    self.window.rootViewController = [FixtureController new];
    [self.window makeKeyAndVisible];
}
@end

int main(int argc, char *argv[]) {
    @autoreleasepool {
        return UIApplicationMain(argc, argv, nil, NSStringFromClass(FixtureDelegate.class));
    }
}

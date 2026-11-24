#import <AVFoundation/AVFoundation.h>
#import <Speech/Speech.h>
typedef void (*MutekiSpeechCallback)(const unsigned char *, size_t);
@interface MutekiSpeechSession : NSObject
@property(nonatomic) MutekiSpeechCallback callback;
@property(nonatomic, strong) SFSpeechRecognizer *recognizer;
@property(nonatomic, strong) SFSpeechAudioBufferRecognitionRequest *request;
@property(nonatomic, strong) SFSpeechRecognitionTask *task;
@property(nonatomic, strong) AVAudioEngine *engine;
@property(nonatomic, strong) NSTimer *recordingTimer;
@property(nonatomic, strong) NSTimer *resultTimer;
@property(nonatomic, copy) NSString *lastPartial;
@property(nonatomic) BOOL finished;
@property(nonatomic) BOOL tapped;
@property(nonatomic) BOOL ending;
- (void)begin:(NSString *)locale;
- (void)endAudio;
- (void)finish:(NSDictionary *)event;
@end

static MutekiSpeechSession *activeSession;

@implementation MutekiSpeechSession
- (void)emit:(NSDictionary *)event {
  if (!self.callback) return;
  NSData *bytes = [NSJSONSerialization dataWithJSONObject:event options:0 error:nil];
  if (!bytes) return;
  self.callback(static_cast<const unsigned char *>(bytes.bytes), bytes.length);
}
- (void)fail:(NSString *)code message:(NSString *)message error:(NSError *)error {
  NSMutableDictionary *event = [@{@"type": @"error", @"code": code, @"message": message} mutableCopy];
  if (error) event[@"detail"] = @{@"domain": error.domain, @"nativeCode": @(error.code), @"description": error.description};
  [self finish:event];
}
- (void)stopCapture {
  [self.recordingTimer invalidate]; self.recordingTimer = nil;
  if (self.engine.isRunning) [self.engine stop];
  if (self.tapped) { [self.engine.inputNode removeTapOnBus:0]; self.tapped = NO; }
}
- (void)finish:(NSDictionary *)event {
  if (self.finished) return;
  self.finished = YES;
  [self stopCapture];
  [self.resultTimer invalidate]; self.resultTimer = nil;
  [self.request endAudio];
  [self.task cancel];
  self.task = nil; self.request = nil; self.engine = nil; self.recognizer = nil;
  [self emit:event];
  self.callback = nullptr;
  if (activeSession == self) activeSession = nil;
}
- (void)endAudio {
  if (self.finished || self.ending) return;
  if (!self.request) { [self finish:@{@"type": @"cancelled"}]; return; }
  self.ending = YES;
  [self stopCapture]; [self.request endAudio]; [self.task finish];
  [self emit:@{@"type": @"processing"}];
  __weak MutekiSpeechSession *weakSelf = self;
  self.resultTimer = [NSTimer scheduledTimerWithTimeInterval:15 repeats:NO block:^(NSTimer *) {
    [weakSelf fail:@"desktop.speech.result_timeout" message:@"macOS 未在结束录音后返回识别结果，请重试。" error:nil];
  }];
}
- (void)recognize:(NSString *)locale {
  if (self.finished) return;
  @try {
    NSLocale *requested = [NSLocale localeWithLocaleIdentifier:locale];
    self.recognizer = [[SFSpeechRecognizer alloc] initWithLocale:requested];
    NSString *wanted = [NSLocale canonicalLocaleIdentifierFromString:requested.localeIdentifier];
    NSString *actual = [NSLocale canonicalLocaleIdentifierFromString:self.recognizer.locale.localeIdentifier ?: @""];
    if (!self.recognizer || ![wanted isEqualToString:actual]) {
      [self fail:@"desktop.speech.locale_unsupported" message:@"macOS 不支持当前语音识别语言。" error:nil]; return;
    }
    if (!self.recognizer.supportsOnDeviceRecognition) {
      [self fail:@"desktop.speech.on_device_unavailable" message:@"当前语言的 macOS 设备端语音识别不可用。请检查系统听写语言与语音资源。" error:nil]; return;
    }
    if (!self.recognizer.isAvailable) {
      [self fail:@"desktop.speech.unavailable" message:@"macOS 语音识别当前不可用，请稍后重试。" error:nil]; return;
    }
    self.recognizer.defaultTaskHint = SFSpeechRecognitionTaskHintDictation;
    self.request = [[SFSpeechAudioBufferRecognitionRequest alloc] init];
    self.request.requiresOnDeviceRecognition = YES;
    self.request.shouldReportPartialResults = YES;
    if (@available(macOS 13.0, *)) self.request.addsPunctuation = YES;
    self.engine = [[AVAudioEngine alloc] init];
    AVAudioInputNode *input = self.engine.inputNode;
    AVAudioFormat *format = [input outputFormatForBus:0];
    if (format.sampleRate <= 0 || format.channelCount == 0) {
      [self fail:@"desktop.speech.audio_unavailable" message:@"没有可用的麦克风输入，请检查系统声音输入设备。" error:nil]; return;
    }
    __weak MutekiSpeechSession *weakSelf = self;
    self.task = [self.recognizer recognitionTaskWithRequest:self.request resultHandler:^(SFSpeechRecognitionResult *result, NSError *error) {
      dispatch_async(dispatch_get_main_queue(), ^{
        MutekiSpeechSession *owner = weakSelf;
        if (!owner || owner.finished) return;
        if (error) { [owner fail:@"desktop.speech.recognition_failed" message:error.localizedDescription error:error]; return; }
        if (!result) return;
        NSString *transcript = result.bestTranscription.formattedString ?: @"";
        if (result.isFinal) {
          if ([transcript stringByTrimmingCharactersInSet:NSCharacterSet.whitespaceAndNewlineCharacterSet].length == 0) {
            if (owner.lastPartial.length) {
              [owner finish:@{@"type": @"result", @"text": owner.lastPartial, @"locale": owner.recognizer.locale.localeIdentifier, @"onDevice": @YES, @"isFinal": @NO}];
            } else {
              [owner fail:@"desktop.speech.no_speech" message:@"未收到语音内容，请重试。" error:nil];
            }
          } else [owner finish:@{@"type": @"result", @"text": transcript, @"locale": owner.recognizer.locale.localeIdentifier, @"onDevice": @YES}];
        } else {
          if ([transcript stringByTrimmingCharactersInSet:NSCharacterSet.whitespaceAndNewlineCharacterSet].length) owner.lastPartial = transcript;
          [owner emit:@{@"type": @"partial", @"text": transcript}];
        }
      });
    }];
    SFSpeechAudioBufferRecognitionRequest *request = self.request;
    [input installTapOnBus:0 bufferSize:1024 format:format block:^(AVAudioPCMBuffer *buffer, AVAudioTime *) { [request appendAudioPCMBuffer:buffer]; }];
    self.tapped = YES;
    [self.engine prepare];
    NSError *error = nil;
    if (![self.engine startAndReturnError:&error]) { [self fail:@"desktop.speech.audio_start_failed" message:error.localizedDescription ?: @"麦克风启动失败。" error:error]; return; }
    [self emit:@{@"type": @"listening", @"locale": self.recognizer.locale.localeIdentifier, @"onDevice": @YES}];
    self.recordingTimer = [NSTimer scheduledTimerWithTimeInterval:55 repeats:NO block:^(NSTimer *) { [weakSelf endAudio]; }];
  } @catch (NSException *exception) {
    [self fail:@"desktop.speech.native_exception" message:[NSString stringWithFormat:@"%@: %@", exception.name, exception.reason ?: @""] error:nil];
  }
}
- (void)authorizeSpeech:(NSString *)locale {
  if (self.finished) return;
  __weak MutekiSpeechSession *weakSelf = self;
  [SFSpeechRecognizer requestAuthorization:^(SFSpeechRecognizerAuthorizationStatus status) {
    dispatch_async(dispatch_get_main_queue(), ^{
      MutekiSpeechSession *owner = weakSelf;
      if (!owner || owner.finished) return;
      if (status != SFSpeechRecognizerAuthorizationStatusAuthorized) {
        [owner fail:status == SFSpeechRecognizerAuthorizationStatusRestricted ? @"desktop.speech.recognition_restricted" : @"desktop.speech.recognition_denied" message:@"macOS 语音识别权限未获允许，请在系统隐私设置中允许 Muteki 使用语音识别。" error:nil]; return;
      }
      [owner recognize:locale];
    });
  }];
}
- (void)begin:(NSString *)locale {
  [self emit:@{@"type": @"requesting"}];
  __weak MutekiSpeechSession *weakSelf = self;
  [AVCaptureDevice requestAccessForMediaType:AVMediaTypeAudio completionHandler:^(BOOL granted) {
    dispatch_async(dispatch_get_main_queue(), ^{
      MutekiSpeechSession *owner = weakSelf;
      if (!owner || owner.finished) return;
      if (!granted) { [owner fail:@"desktop.speech.microphone_denied" message:@"麦克风权限未获允许，请在系统隐私设置中允许 Muteki 使用麦克风。" error:nil]; return; }
      [owner authorizeSpeech:locale];
    });
  }];
}
@end


extern "C" int muteki_speech_start(const char *locale, MutekiSpeechCallback callback) {
  if (![[NSBundle.mainBundle objectForInfoDictionaryKey:@"NSMicrophoneUsageDescription"] length] || ![[NSBundle.mainBundle objectForInfoDictionaryKey:@"NSSpeechRecognitionUsageDescription"] length]) return 1;
  if (activeSession) return 2;
  NSString *language = [NSString stringWithUTF8String:locale]; if (!language.length || !callback) return 3;
  MutekiSpeechSession *owner = [[MutekiSpeechSession alloc] init]; owner.callback = callback; activeSession = owner;
  dispatch_async(dispatch_get_main_queue(), ^{ if (!owner.finished) [owner begin:language]; }); return 0;
}
extern "C" void muteki_speech_finish() { [activeSession endAudio]; }
extern "C" void muteki_speech_cancel() { [activeSession finish:@{@"type": @"cancelled"}]; }

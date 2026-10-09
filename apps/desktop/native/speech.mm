#define NAPI_VERSION 8
#include <node_api.h>
#include <string>
#import <AVFoundation/AVFoundation.h>
#import <Speech/Speech.h>
#import <UserNotifications/UserNotifications.h>

// Node-API keeps this module independent of Electron's Node/V8 ABI. All Cocoa
// state belongs to the main queue; only the TSFN crosses into JavaScript.
@interface MutekiSpeechSession : NSObject
@property(nonatomic) napi_threadsafe_function callback;
@property(nonatomic, strong) SFSpeechRecognizer *recognizer;
@property(nonatomic, strong) SFSpeechAudioBufferRecognitionRequest *request;
@property(nonatomic, strong) SFSpeechRecognitionTask *task;
@property(nonatomic, strong) AVAudioEngine *engine;
@property(nonatomic, strong) NSTimer *recordingTimer;
@property(nonatomic, strong) NSTimer *resultTimer;
@property(nonatomic) BOOL finished;
@property(nonatomic) BOOL tapped;
@property(nonatomic) BOOL ending;
- (void)begin:(NSString *)locale;
- (void)endAudio;
- (void)finish:(NSDictionary *)event;
@end

static MutekiSpeechSession *activeSession;

static void Deliver(napi_env env, napi_value callback, void *, void *data) {
  auto *json = static_cast<std::string *>(data);
  if (env && callback) {
    napi_value value, receiver, result;
    napi_create_string_utf8(env, json->data(), json->size(), &value);
    napi_get_undefined(env, &receiver);
    napi_call_function(env, receiver, callback, 1, &value, &result);
  }
  delete json;
}

@implementation MutekiSpeechSession
- (void)emit:(NSDictionary *)event {
  if (!self.callback) return;
  NSData *bytes = [NSJSONSerialization dataWithJSONObject:event options:0 error:nil];
  if (!bytes) return;
  auto *json = new std::string(static_cast<const char *>(bytes.bytes), bytes.length);
  if (napi_call_threadsafe_function(self.callback, json, napi_tsfn_nonblocking) != napi_ok) delete json;
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
  if (self.callback) { napi_release_threadsafe_function(self.callback, napi_tsfn_release); self.callback = nullptr; }
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
            [owner fail:@"desktop.speech.no_speech" message:@"未收到语音内容，请重试。" error:nil];
          } else [owner finish:@{@"type": @"result", @"text": transcript, @"locale": owner.recognizer.locale.localeIdentifier, @"onDevice": @YES}];
        } else [owner emit:@{@"type": @"partial", @"text": transcript}];
      });
    }];
    // The tap owns only the request. Removing it precedes request disposal.
    SFSpeechAudioBufferRecognitionRequest *request = self.request;
    [input installTapOnBus:0 bufferSize:1024 format:format block:^(AVAudioPCMBuffer *buffer, AVAudioTime *) { [request appendAudioPCMBuffer:buffer]; }];
    self.tapped = YES;
    [self.engine prepare];
    NSError *error = nil;
    if (![self.engine startAndReturnError:&error]) { [self fail:@"desktop.speech.audio_start_failed" message:error.localizedDescription ?: @"麦克风启动失败。" error:error]; return; }
    [self emit:@{@"type": @"listening", @"locale": self.recognizer.locale.localeIdentifier, @"onDevice": @YES}];
    // Apple's live recognition has a one-minute limit. Finalize before it,
    // preserving the service's actual final result instead of inventing one.
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

static napi_value Start(napi_env env, napi_callback_info info) {
  if (![[NSBundle.mainBundle objectForInfoDictionaryKey:@"NSMicrophoneUsageDescription"] length] || ![[NSBundle.mainBundle objectForInfoDictionaryKey:@"NSSpeechRecognitionUsageDescription"] length]) {
    napi_throw_error(env, "desktop.speech.usage_description_missing", "原生语音识别需要包含麦克风与语音识别用途声明的 macOS 应用，请运行打包后的 Muteki。"); return nullptr;
  }
  size_t argc = 2; napi_value args[2], resource;
  napi_get_cb_info(env, info, &argc, args, nullptr, nullptr);
  napi_valuetype first, second;
  if (argc != 2 || napi_typeof(env, args[0], &first) != napi_ok || first != napi_string || napi_typeof(env, args[1], &second) != napi_ok || second != napi_function) {
    napi_throw_type_error(env, nullptr, "start requires a locale and callback"); return nullptr;
  }
  if (activeSession) { napi_throw_error(env, "desktop.speech.busy", "Another native recording is active"); return nullptr; }
  size_t length = 0; napi_get_value_string_utf8(env, args[0], nullptr, 0, &length);
  std::string locale(length + 1, '\0'); napi_get_value_string_utf8(env, args[0], locale.data(), locale.size(), &length); locale.resize(length);
  NSString *language = [[NSString alloc] initWithBytes:locale.data() length:locale.size() encoding:NSUTF8StringEncoding];
  if (!language.length) { napi_throw_type_error(env, nullptr, "A valid locale is required"); return nullptr; }
  napi_create_string_utf8(env, "Muteki macOS speech", NAPI_AUTO_LENGTH, &resource);
  napi_threadsafe_function callback;
  if (napi_create_threadsafe_function(env, args[1], nullptr, resource, 0, 1, nullptr, nullptr, nullptr, Deliver, &callback) != napi_ok) {
    napi_throw_error(env, "desktop.speech.bridge_failed", "无法建立原生语音结果通道。"); return nullptr;
  }
  MutekiSpeechSession *owner = [[MutekiSpeechSession alloc] init]; owner.callback = callback; activeSession = owner;
  dispatch_async(dispatch_get_main_queue(), ^{ if (!owner.finished) [owner begin:language]; });
  return nullptr;
}
static napi_value Finish(napi_env, napi_callback_info) { [activeSession endAudio]; return nullptr; }
static napi_value Cancel(napi_env, napi_callback_info) { [activeSession finish:@{@"type": @"cancelled"}]; return nullptr; }
static void Cleanup(void *) { [activeSession finish:@{@"type": @"cancelled"}]; }
static napi_value NotificationSettings(napi_env env, napi_callback_info info) {
  size_t argc = 1; napi_value args[1], resource; napi_valuetype type;
  napi_get_cb_info(env, info, &argc, args, nullptr, nullptr);
  if (argc != 1 || napi_typeof(env, args[0], &type) != napi_ok || type != napi_function) {
    napi_throw_type_error(env, nullptr, "notificationSettings requires a callback"); return nullptr;
  }
  napi_create_string_utf8(env, "Muteki notification settings", NAPI_AUTO_LENGTH, &resource);
  napi_threadsafe_function callback;
  if (napi_create_threadsafe_function(env, args[0], nullptr, resource, 0, 1, nullptr, nullptr, nullptr, Deliver, &callback) != napi_ok) {
    napi_throw_error(env, "desktop.notification_status_bridge_failed", "Cannot create notification settings callback"); return nullptr;
  }
  auto deliver = ^(NSDictionary *result) {
    NSData *bytes = [NSJSONSerialization dataWithJSONObject:result options:0 error:nil];
    auto *json = new std::string(static_cast<const char *>(bytes.bytes), bytes.length);
    if (napi_call_threadsafe_function(callback, json, napi_tsfn_nonblocking) != napi_ok) delete json;
    napi_release_threadsafe_function(callback, napi_tsfn_release);
  };
  @try {
    // Read only. Never call requestAuthorization or schedule a notification.
    [UNUserNotificationCenter.currentNotificationCenter getNotificationSettingsWithCompletionHandler:^(UNNotificationSettings *settings) {
      NSString *status;
      switch (settings.authorizationStatus) {
        case UNAuthorizationStatusNotDetermined: status = @"notDetermined"; break;
        case UNAuthorizationStatusDenied: status = @"denied"; break;
        case UNAuthorizationStatusAuthorized: status = @"authorized"; break;
        case UNAuthorizationStatusProvisional: status = @"provisional"; break;
        default: status = @"unknown"; break;
      }
      deliver(@{@"authorizationStatus": status, @"authorizationStatusRaw": @(settings.authorizationStatus),
        @"alertSettingRaw": @(settings.alertSetting), @"soundSettingRaw": @(settings.soundSetting), @"notificationCenterSettingRaw": @(settings.notificationCenterSetting), @"lockScreenSettingRaw": @(settings.lockScreenSetting), @"alertStyleRaw": @(settings.alertStyle)});
    }];
  } @catch (NSException *exception) {
    deliver(@{@"error": @{@"code": @"desktop.notification_status_failed", @"message": exception.reason ?: exception.description, @"nativeName": exception.name}});
  }
  return nullptr;
}
static napi_value Init(napi_env env, napi_value exports) {
  napi_property_descriptor methods[] = {{"start", nullptr, Start, nullptr, nullptr, nullptr, napi_default, nullptr}, {"finish", nullptr, Finish, nullptr, nullptr, nullptr, napi_default, nullptr}, {"cancel", nullptr, Cancel, nullptr, nullptr, nullptr, napi_default, nullptr}, {"notificationSettings", nullptr, NotificationSettings, nullptr, nullptr, nullptr, napi_default, nullptr}};
  napi_define_properties(env, exports, 4, methods); napi_add_env_cleanup_hook(env, Cleanup, nullptr); return exports;
}
NAPI_MODULE(NODE_GYP_MODULE_NAME, Init)

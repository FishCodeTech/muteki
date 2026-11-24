import AVFoundation
import Speech
import UIKit

private enum DictationFailure: Error {
    case permissionDenied
    case unsupported
}

@MainActor
private enum Dictation {
    /// A capturing session keeps the screen on: the idle timer would
    /// otherwise lock the phone mid-dictation, which ends the capture.
    static var sessions: [UInt64: NativeDictation] = [:] {
        didSet { UIApplication.shared.isIdleTimerDisabled = !sessions.isEmpty }
    }
    static var availability: [String: Bool] = [:]
    static var cleanup: Task<Void, Never>?
    static var permissionsAllowed: Bool?
    static let permissionObserver = NotificationCenter.default.addObserver(
        forName: UIApplication.didBecomeActiveNotification, object: nil, queue: .main
    ) { _ in
        MainActor.assumeIsolated {
            refreshPermissions()
            tcode_ios_speech_availability_changed()
        }
    }

    static func refreshPermissions() {
        let speech = SFSpeechRecognizer.authorizationStatus()
        permissionsAllowed = speech != .denied && speech != .restricted
            && AVAudioApplication.shared.recordPermission != .denied
    }

    static func available(_ language: String) -> Bool {
        _ = permissionObserver
        if permissionsAllowed == nil { refreshPermissions() }
        if availability[language] == nil {
            availability[language] = false
            Task {
                if let transcription = await Transcription.make(language) {
                    availability[language] = await AssetInventory.status(forModules: [transcription.module]) != .unsupported
                }
                tcode_ios_speech_availability_changed()
            }
        }
        return availability[language] == true && permissionsAllowed == true
    }
}

private final class DictationAudio: @unchecked Sendable {
    let converter: AVAudioConverter
    let continuation: AsyncThrowingStream<AnalyzerInput, Error>.Continuation

    init?(from input: AVAudioFormat, to output: AVAudioFormat,
          continuation: AsyncThrowingStream<AnalyzerInput, Error>.Continuation) {
        guard let converter = AVAudioConverter(from: input, to: output) else { return nil }
        self.converter = converter
        self.continuation = continuation
    }

    func append(_ input: AVAudioPCMBuffer) {
        let capacity = AVAudioFrameCount(ceil(Double(input.frameLength) * converter.outputFormat.sampleRate / input.format.sampleRate)) + 1
        guard let output = AVAudioPCMBuffer(pcmFormat: converter.outputFormat, frameCapacity: capacity) else { return }
        var supplied = false
        var error: NSError?
        let status = converter.convert(to: output, error: &error) { _, state in
            if supplied { state.pointee = .noDataNow; return nil }
            supplied = true
            state.pointee = .haveData
            return input
        }
        if status == .error {
            continuation.finish(throwing: error ?? CocoaError(.coderInvalidValue))
        } else if output.frameLength > 0 {
            // The engine reuses tap buffers; the converted buffer owns these samples.
            continuation.yield(AnalyzerInput(buffer: output))
        }
    }
}

@MainActor
private final class NativeDictation {
    let id: UInt64
    let language: String
    private var engine: AVAudioEngine?
    private var analyzer: SpeechAnalyzer?
    private var continuation: AsyncThrowingStream<AnalyzerInput, Error>.Continuation?
    private var operation: Task<Void, Never>?
    private var results: Task<Void, Error>?
    private var finalization: Task<Void, Never>?
    private var observers: [NSObjectProtocol] = []
    private var reservedLocale: Locale?
    private var audioSessionActive = false
    private var ended = false
    private var stopping = false

    init(id: UInt64, language: String) { self.id = id; self.language = language }

    func start() {
        for notification in [UIApplication.didEnterBackgroundNotification,
                             AVAudioSession.interruptionNotification,
                             AVAudioSession.mediaServicesWereResetNotification] {
            observers.append(NotificationCenter.default.addObserver(forName: notification, object: nil, queue: .main) { [weak self] _ in
                MainActor.assumeIsolated { self?.complete() }
            })
        }
        let pendingCleanup = Dictation.cleanup
        operation = Task { [self] in
            do {
                await pendingCleanup?.value
                try Task.checkCancellation()
                let speech = await withCheckedContinuation { continuation in
                    SFSpeechRecognizer.requestAuthorization { continuation.resume(returning: $0) }
                }
                Dictation.refreshPermissions()
                tcode_ios_speech_availability_changed()
                try Task.checkCancellation()
                guard speech == .authorized else { throw DictationFailure.permissionDenied }
                let microphone = await AVAudioApplication.requestRecordPermission()
                Dictation.refreshPermissions()
                tcode_ios_speech_availability_changed()
                try Task.checkCancellation()
                guard microphone else { throw DictationFailure.permissionDenied }
                guard let transcription = await Transcription.make(language) else {
                    throw DictationFailure.unsupported
                }
                let locale = transcription.locale
                let module = transcription.module
                try Task.checkCancellation()
                try await AssetInventory.reserve(locale: locale)
                if Task.isCancelled {
                    await AssetInventory.release(reservedLocale: locale)
                    throw CancellationError()
                }
                reservedLocale = locale
                if let installation = try await AssetInventory.assetInstallationRequest(supporting: [module]) {
                    try Task.checkCancellation()
                    try await installation.downloadAndInstall()
                }
                try Task.checkCancellation()
                let session = AVAudioSession.sharedInstance()
                try session.setCategory(.record, mode: .measurement, options: [.duckOthers])
                try session.setActive(true)
                audioSessionActive = true
                let engine = AVAudioEngine()
                let inputFormat = engine.inputNode.outputFormat(forBus: 0)
                guard inputFormat.sampleRate > 0, inputFormat.channelCount > 0,
                      let format = await SpeechAnalyzer.bestAvailableAudioFormat(compatibleWith: [module], considering: inputFormat) else {
                    throw DictationFailure.unsupported
                }
                try Task.checkCancellation()
                let analyzer = SpeechAnalyzer(modules: [module])
                self.analyzer = analyzer
                try await analyzer.prepareToAnalyze(in: format)
                try Task.checkCancellation()
                let (input, continuation) = AsyncThrowingStream<AnalyzerInput, Error>.makeStream()
                self.continuation = continuation
                guard let audio = DictationAudio(from: inputFormat, to: format, continuation: continuation) else {
                    throw DictationFailure.unsupported
                }
                results = Task { [self] in
                    try await transcription.results { text, isFinal in
                        if !Task.isCancelled { self.emit(isFinal ? 2 : 1, text) }
                    }
                }
                try await analyzer.start(inputSequence: input)
                try Task.checkCancellation()
                engine.inputNode.installTap(onBus: 0, bufferSize: 1024, format: inputFormat) { buffer, _ in audio.append(buffer) }
                self.engine = engine
                engine.prepare()
                try engine.start()
                emit(0)
                try await results?.value
                complete()
            } catch {
                if !ended { complete(error: error) }
            }
        }
    }

    func stop(finish: Bool) {
        guard !ended else { return }
        guard finish, let analyzer, engine != nil else {
            complete(cancelled: !finish)
            return
        }
        guard !stopping else { return }
        stopping = true
        stopCapture()
        finalization = Task { [self] in
            do { try await analyzer.finalizeAndFinishThroughEndOfInput() }
            catch { complete(error: error) }
        }
    }

    func preempt() { complete() }

    private func stopCapture() {
        if let engine {
            engine.stop()
            engine.inputNode.removeTap(onBus: 0)
            self.engine = nil
        }
        continuation?.finish()
        continuation = nil
        if audioSessionActive {
            try? AVAudioSession.sharedInstance().setActive(false, options: .notifyOthersOnDeactivation)
            audioSessionActive = false
        }
    }

    private func complete(error: Error? = nil, cancelled: Bool = false) {
        guard !ended else { return }
        ended = true
        stopCapture()
        observers.forEach(NotificationCenter.default.removeObserver)
        observers.removeAll()
        let operation = self.operation
        operation?.cancel()
        self.operation = nil
        results?.cancel()
        results = nil
        finalization?.cancel()
        finalization = nil
        let analyzer = self.analyzer
        self.analyzer = nil
        let locale = reservedLocale
        reservedLocale = nil
        let pendingCleanup = Dictation.cleanup
        Dictation.cleanup = Task {
            await analyzer?.cancelAndFinishNow()
            await operation?.value
            await pendingCleanup?.value
            if let locale { await AssetInventory.release(reservedLocale: locale) }
        }
        if !cancelled {
            if let failure = error as? DictationFailure {
                switch failure {
                case .permissionDenied: emit(5)
                case .unsupported: emit(6)
                }
            } else {
                emit(error == nil ? 3 : 4, error?.localizedDescription ?? "")
            }
        }
        Dictation.sessions.removeValue(forKey: id)
    }

    private func emit(_ kind: UInt32, _ text: String = "") {
        withUTF8(text) { tcode_ios_speech_event(id, kind, $0, $1) }
    }
}

private func speechLanguage(_ bytes: UnsafePointer<UInt8>?, _ length: Int) -> String {
    guard let bytes else { return "" }
    return String(decoding: UnsafeBufferPointer(start: bytes, count: length), as: UTF8.self)
}

@_cdecl("tcode_ios_host_speech_available")
public func tcodeSpeechAvailable(_ bytes: UnsafePointer<UInt8>?, _ length: Int) -> UInt8 {
    MainActor.assumeIsolated { Dictation.available(speechLanguage(bytes, length)) ? 1 : 0 }
}

@_cdecl("tcode_ios_host_speech_start")
public func tcodeSpeechStart(_ id: UInt64, _ bytes: UnsafePointer<UInt8>?, _ length: Int) {
    MainActor.assumeIsolated {
        // Only one session may own AVAudioSession's microphone at a time.
        for session in Array(Dictation.sessions.values) { session.preempt() }
        let session = NativeDictation(id: id, language: speechLanguage(bytes, length))
        Dictation.sessions[id] = session
        session.start()
    }
}

@_cdecl("tcode_ios_host_speech_stop")
public func tcodeSpeechStop(_ id: UInt64, _ finish: UInt8) {
    MainActor.assumeIsolated { Dictation.sessions[id]?.stop(finish: finish != 0) }
}

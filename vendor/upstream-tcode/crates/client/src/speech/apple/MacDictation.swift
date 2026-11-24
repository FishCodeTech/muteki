import AVFoundation
import Speech

/// `context`, event kind, UTF-8 text or samples, their length, sample rate.
public typealias SpeechCallback = @convention(c) (UnsafeRawPointer?, UInt32, UnsafeRawPointer?, Int, Double) -> Void
public typealias SpeechRelease = @convention(c) (UnsafeRawPointer?) -> Void

private enum Kind {
    static let ready: UInt32 = 0
    static let hypothesis: UInt32 = 1
    static let phrase: UInt32 = 2
    static let finished: UInt32 = 3
    static let error: UInt32 = 4
    static let noInputDevice: UInt32 = 5
    static let unsupported: UInt32 = 6
    static let samples: UInt32 = 7
}

private enum DictationFailure: Error {
    case noInputDevice
    case unsupported
}

/// The Rust session's end of the callbacks, released once nothing can call it.
private final class Sink: @unchecked Sendable {
    let context: UnsafeRawPointer?
    let callback: SpeechCallback
    let release: SpeechRelease

    init(context: UnsafeRawPointer?, callback: @escaping SpeechCallback, release: @escaping SpeechRelease) {
        self.context = context
        self.callback = callback
        self.release = release
    }

    deinit { release(context) }

    func emit(_ kind: UInt32, _ text: String = "") {
        var text = text
        text.withUTF8 { callback(context, kind, UnsafeRawPointer($0.baseAddress), $0.count, 0) }
    }

    func samples(_ buffer: AVAudioPCMBuffer) {
        guard let channels = buffer.floatChannelData else { return }
        callback(context, Kind.samples, UnsafeRawPointer(channels[0]), Int(buffer.frameLength), buffer.format.sampleRate)
    }
}

private final class DictationAudio: @unchecked Sendable {
    let converter: AVAudioConverter
    let continuation: AsyncThrowingStream<AnalyzerInput, Error>.Continuation
    let sink: Sink

    init?(from input: AVAudioFormat, to output: AVAudioFormat, sink: Sink,
          continuation: AsyncThrowingStream<AnalyzerInput, Error>.Continuation) {
        guard let converter = AVAudioConverter(from: input, to: output) else { return nil }
        self.converter = converter
        self.continuation = continuation
        self.sink = sink
    }

    func append(_ input: AVAudioPCMBuffer) {
        sink.samples(input)
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
private final class MacDictation {
    static var sessions: [UInt64: MacDictation] = [:]
    static var cleanup: Task<Void, Never>?

    let id: UInt64
    let language: String
    let sink: Sink
    private var engine: AVAudioEngine?
    private var analyzer: SpeechAnalyzer?
    private var continuation: AsyncThrowingStream<AnalyzerInput, Error>.Continuation?
    private var operation: Task<Void, Never>?
    private var results: Task<Void, Error>?
    private var finalization: Task<Void, Never>?
    private var reservedLocale: Locale?
    private var ended = false
    private var stopping = false

    init(id: UInt64, language: String, sink: Sink) {
        self.id = id
        self.language = language
        self.sink = sink
    }

    func start() {
        let pendingCleanup = Self.cleanup
        operation = Task { [self] in
            do {
                await pendingCleanup?.value
                try Task.checkCancellation()
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
                let engine = AVAudioEngine()
                let inputFormat = engine.inputNode.outputFormat(forBus: 0)
                guard inputFormat.sampleRate > 0, inputFormat.channelCount > 0 else {
                    throw DictationFailure.noInputDevice
                }
                guard let format = await SpeechAnalyzer.bestAvailableAudioFormat(compatibleWith: [module], considering: inputFormat) else {
                    throw DictationFailure.unsupported
                }
                try Task.checkCancellation()
                let analyzer = SpeechAnalyzer(modules: [module])
                self.analyzer = analyzer
                try await analyzer.prepareToAnalyze(in: format)
                try Task.checkCancellation()
                let (input, continuation) = AsyncThrowingStream<AnalyzerInput, Error>.makeStream()
                self.continuation = continuation
                guard let audio = DictationAudio(from: inputFormat, to: format, sink: sink, continuation: continuation) else {
                    throw DictationFailure.unsupported
                }
                results = Task { [self] in
                    try await transcription.results { text, isFinal in
                        if !Task.isCancelled { self.sink.emit(isFinal ? Kind.phrase : Kind.hypothesis, text) }
                    }
                }
                try await analyzer.start(inputSequence: input)
                try Task.checkCancellation()
                engine.inputNode.installTap(onBus: 0, bufferSize: 1280, format: inputFormat) { buffer, _ in audio.append(buffer) }
                self.engine = engine
                engine.prepare()
                try engine.start()
                sink.emit(Kind.ready)
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
    }

    private func complete(error: Error? = nil, cancelled: Bool = false) {
        guard !ended else { return }
        ended = true
        stopCapture()
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
        let pendingCleanup = Self.cleanup
        Self.cleanup = Task {
            await analyzer?.cancelAndFinishNow()
            await operation?.value
            await pendingCleanup?.value
            if let locale { await AssetInventory.release(reservedLocale: locale) }
        }
        if !cancelled {
            switch (error, error as? DictationFailure) {
            case (nil, _): sink.emit(Kind.finished)
            case (_, .noInputDevice?): sink.emit(Kind.noInputDevice)
            case (_, .unsupported?): sink.emit(Kind.unsupported)
            case let (error?, nil): sink.emit(Kind.error, error.localizedDescription)
            }
        }
        Self.sessions.removeValue(forKey: id)
    }
}

/// Whether some transcriber dictates `language` on this Mac. The answer is
/// cached, so only the first query per language waits for Speech.
private enum Availability {
    nonisolated(unsafe) static var languages: [String: Bool] = [:]
    static let lock = NSLock()

    static func check(_ language: String) -> Bool {
        if let known = lock.withLock({ languages[language] }) { return known }
        let done = DispatchSemaphore(value: 0)
        let result = Box()
        Task.detached {
            if let transcription = await Transcription.make(language) {
                result.value = await AssetInventory.status(forModules: [transcription.module]) != .unsupported
            }
            done.signal()
        }
        done.wait()
        lock.withLock { languages[language] = result.value }
        return result.value
    }

    private final class Box: @unchecked Sendable { var value = false }
}

private func speechLanguage(_ bytes: UnsafePointer<UInt8>?, _ length: Int) -> String {
    guard let bytes else { return "" }
    return String(decoding: UnsafeBufferPointer(start: bytes, count: length), as: UTF8.self)
}

@_cdecl("tcode_macos_speech_available")
public func tcodeMacSpeechAvailable(_ bytes: UnsafePointer<UInt8>?, _ length: Int) -> UInt8 {
    Availability.check(speechLanguage(bytes, length)) ? 1 : 0
}

/// Starts a session that reports to `callback` with `context` until it ends,
/// then calls `release` with `context` once.
@_cdecl("tcode_macos_speech_start")
public func tcodeMacSpeechStart(_ id: UInt64, _ bytes: UnsafePointer<UInt8>?, _ length: Int,
                                _ context: UnsafeRawPointer?, _ callback: SpeechCallback,
                                _ release: SpeechRelease) {
    let sink = Sink(context: context, callback: callback, release: release)
    let language = speechLanguage(bytes, length)
    MainActor.assumeIsolated {
        // One AVAudioEngine input tap at a time.
        for session in Array(MacDictation.sessions.values) { session.preempt() }
        let session = MacDictation(id: id, language: language, sink: sink)
        MacDictation.sessions[id] = session
        session.start()
    }
}

@_cdecl("tcode_macos_speech_stop")
public func tcodeMacSpeechStop(_ id: UInt64, _ finish: UInt8) {
    MainActor.assumeIsolated { MacDictation.sessions[id]?.stop(finish: finish != 0) }
}

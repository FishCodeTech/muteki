import Speech

/// The on-device transcriber dictation uses for a language. Shared by the macOS
/// client and the iOS host.
struct Transcription {
    let locale: Locale
    let module: any SpeechModule
    /// Reports each result's text and whether it is final until the analyzer
    /// finishes. A result's text starts with the space it needs after the
    /// previous one.
    let results: @MainActor (@escaping @MainActor (String, Bool) -> Void) async throws -> Void

    /// SpeechTranscriber keeps far more of what was said than
    /// DictationTranscriber or SFSpeechRecognizer, which transcribe alike,
    /// in noise and with English terms inside Chinese, at the cost of more
    /// misheard words; a draft that an agent reads is better long and rough
    /// than short and clean. Its `fastResults` option lowers what it keeps, so
    /// only volatile results are requested. Hardware without SpeechTranscriber
    /// falls back to DictationTranscriber.
    static func make(_ language: String) async -> Transcription? {
        let requested = Locale(identifier: language)
        if SpeechTranscriber.isAvailable,
           let locale = await SpeechTranscriber.supportedLocale(equivalentTo: requested) {
            let module = SpeechTranscriber(locale: locale, transcriptionOptions: [],
                                           reportingOptions: [.volatileResults], attributeOptions: [])
            return Transcription(locale: locale, module: module) { @MainActor report in
                for try await result in module.results { report(String(result.text.characters), result.isFinal) }
            }
        }
        if let locale = await DictationTranscriber.supportedLocale(equivalentTo: requested) {
            let module = DictationTranscriber(locale: locale, preset: .progressiveLongDictation)
            return Transcription(locale: locale, module: module) { @MainActor report in
                for try await result in module.results { report(String(result.text.characters), result.isFinal) }
            }
        }
        return nil
    }
}

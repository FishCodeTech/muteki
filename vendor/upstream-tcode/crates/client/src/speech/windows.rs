// Copyright 2024 - 2026 Longbridge <https://longbridge.com>
// Licensed under Apache-2.0; see LICENSE-APACHE.
// Modified for Tcode client sessions and native capture.

//! Windows dictation adapted from gpui-component 0.7.1 (Apache-2.0).
//! WinRT owns capture and requires the online speech privacy setting.
//! The caller must initialize a COM apartment; the desktop GPUI platform does.

use async_channel::{Receiver, Sender, unbounded};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use windows::{
    Foundation::{TimeSpan, TypedEventHandler},
    Globalization::Language,
    Media::SpeechRecognition::{
        SpeechContinuousRecognitionCompletedEventArgs,
        SpeechContinuousRecognitionResultGeneratedEventArgs, SpeechContinuousRecognitionSession,
        SpeechRecognitionHypothesisGeneratedEventArgs, SpeechRecognitionResult,
        SpeechRecognitionResultStatus, SpeechRecognitionScenario, SpeechRecognitionTopicConstraint,
        SpeechRecognizer as WinSpeechRecognizer,
    },
    Win32::Foundation::E_ACCESSDENIED,
    core::{HRESULT, HSTRING, Ref},
};

use super::{SpeechError, SpeechEvent, SpeechRecognizer, SpeechSession};
type SpeechSink = Sender<SpeechEvent>;

/// `SPERR_SPEECH_PRIVACY_POLICY_NOT_ACCEPTED`: "Online speech recognition" is
/// turned off in the privacy settings, which dictation requires.
const PRIVACY_POLICY_NOT_ACCEPTED: HRESULT = HRESULT(0x80045509_u32 as i32);
/// `MF_E_NO_CAPTURE_DEVICES_AVAILABLE`.
const NO_CAPTURE_DEVICES: HRESULT = HRESULT(0xC00DABE0_u32 as i32);
/// Ten minutes, in the 100 ns ticks of a `TimeSpan`.
const AUTO_STOP_SILENCE: TimeSpan = TimeSpan {
    Duration: 10 * 60 * 10_000_000,
};

pub struct SystemRecognizer {
    /// The dictation language, or `None` when the requested one is malformed or
    /// has no speech pack installed.
    language: Option<Language>,
    separator: &'static str,
}

impl SystemRecognizer {
    pub fn new(locale: &str) -> Self {
        let language = supported_language(Some(locale))
            .inspect_err(|error| log::warn!("speech: no dictation language: {error:#}"))
            .ok()
            .flatten();
        let separator = language
            .as_ref()
            .and_then(|language| language.LanguageTag().ok())
            .map_or(" ", |tag| phrase_separator(&tag.to_string_lossy()));
        Self {
            language,
            separator,
        }
    }
}

impl SpeechRecognizer for SystemRecognizer {
    fn is_available(&self, _: &str) -> bool {
        self.language.is_some()
    }

    fn start(&self, _: &str, sink: SpeechSink) -> Result<Box<dyn SpeechSession>, SpeechError> {
        let Some(language) = &self.language else {
            return Err(SpeechError::Unsupported);
        };

        let recognizer = WinSpeechRecognizer::Create(language).map_err(speech_error)?;
        let continuous = recognizer
            .ContinuousRecognitionSession()
            .map_err(speech_error)?;
        // The user ends dictation; a pause to think must not end it for them
        // and drop what they say next.
        if let Err(error) = continuous.SetAutoStopSilenceTimeout(AUTO_STOP_SILENCE) {
            log::warn!("speech: could not extend the silence timeout: {error}");
        }
        let (messages, rx) = unbounded();
        let mut session = Session {
            recognizer: recognizer.clone(),
            continuous: continuous.clone(),
            hypothesis_token: None,
            result_token: None,
            completed_token: None,
            messages: messages.clone(),
            _task: None,
            levels: None,
        };

        let constraint = SpeechRecognitionTopicConstraint::Create(
            SpeechRecognitionScenario::Dictation,
            &HSTRING::from("dictation"),
        )
        .map_err(speech_error)?;
        recognizer
            .Constraints()
            .and_then(|constraints| constraints.Append(&constraint))
            .map_err(speech_error)?;

        session.hypothesis_token = Some(
            recognizer
                .HypothesisGenerated(&TypedEventHandler::new({
                    let messages = messages.clone();
                    move |_, args: Ref<SpeechRecognitionHypothesisGeneratedEventArgs>| {
                        if let Some(args) = args.as_ref() {
                            let text = args.Hypothesis()?.Text()?;
                            _ = messages.try_send(Message::Hypothesis(text));
                        }
                        Ok(())
                    }
                }))
                .map_err(speech_error)?,
        );
        session.result_token = Some(
            continuous
                .ResultGenerated(&TypedEventHandler::new({
                    let messages = messages.clone();
                    move |_, args: Ref<SpeechContinuousRecognitionResultGeneratedEventArgs>| {
                        if let Some(args) = args.as_ref() {
                            _ = messages.try_send(Message::Result(args.Result()?));
                        }
                        Ok(())
                    }
                }))
                .map_err(speech_error)?,
        );
        session.completed_token = Some(
            continuous
                .Completed(&TypedEventHandler::new({
                    let messages = messages.clone();
                    move |_, args: Ref<SpeechContinuousRecognitionCompletedEventArgs>| {
                        if let Some(args) = args.as_ref() {
                            _ = messages.try_send(Message::Completed(args.Status()?));
                        }
                        Ok(())
                    }
                }))
                .map_err(speech_error)?,
        );

        session.levels = Some(microphone_levels(sink.clone())?);
        let separator = self.separator;
        session._task = Some(Box::pin(async move {
            let result = dictate(&recognizer, &continuous, rx, &sink, separator).await;
            if let Err(error) = result {
                _ = sink.try_send(SpeechEvent::Error(error));
            }
        }));
        Ok(Box::new(session))
    }
}

/// What the WinRT threads and [`Session::finish`] tell the dictation task.
enum Message {
    Hypothesis(HSTRING),
    Result(SpeechRecognitionResult),
    Completed(SpeechRecognitionResultStatus),
    /// The user stopped talking.
    Finish,
}

struct Session {
    recognizer: WinSpeechRecognizer,
    continuous: SpeechContinuousRecognitionSession,
    hypothesis_token: Option<i64>,
    result_token: Option<i64>,
    completed_token: Option<i64>,
    messages: Sender<Message>,
    _task: Option<Pin<Box<dyn Future<Output = ()>>>>,
    levels: Option<cpal::Stream>,
}

impl SpeechSession for Session {
    fn finish(&mut self) {
        self.levels = None;
        _ = self.messages.try_send(Message::Finish);
    }
    fn poll(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        self._task.as_mut().unwrap().as_mut().poll(cx)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.levels = None;
        if let Some(token) = self.hypothesis_token.take() {
            _ = self.recognizer.RemoveHypothesisGenerated(token);
        }
        if let Some(token) = self.result_token.take() {
            _ = self.continuous.RemoveResultGenerated(token);
        }
        if let Some(token) = self.completed_token.take() {
            _ = self.continuous.RemoveCompleted(token);
        }

        // Close the recognizer once the cancellation lands, or right away when
        // there is nothing to cancel.
        let recognizer = self.recognizer.clone();
        let closed = self.continuous.CancelAsync().and_then(|action| {
            action.when(move |_| {
                _ = recognizer.Close();
            })
        });
        if closed.is_err() {
            _ = self.recognizer.Close();
        }
    }
}

/// Compile the dictation constraint, start the continuous session and report
/// its results to `sink` until it completes.
async fn dictate(
    recognizer: &WinSpeechRecognizer,
    continuous: &SpeechContinuousRecognitionSession,
    messages: Receiver<Message>,
    sink: &SpeechSink,
    separator: &'static str,
) -> Result<(), SpeechError> {
    let compilation = recognizer
        .CompileConstraintsAsync()
        .map_err(speech_error)?
        .await
        .map_err(speech_error)?;
    let status = compilation.Status().map_err(speech_error)?;
    if status != SpeechRecognitionResultStatus::Success {
        return Err(status_error(status));
    }
    continuous
        .StartAsync()
        .map_err(speech_error)?
        .await
        .map_err(speech_error)?;
    _ = sink.try_send(SpeechEvent::Ready);

    let mut separator_due = false;
    while let Ok(message) = messages.recv().await {
        match message {
            Message::Hypothesis(text) => {
                let text = joined(separator_due, separator, &text);
                _ = sink.try_send(SpeechEvent::Hypothesis(text));
            }
            Message::Result(result) => {
                let Some(text) = phrase_text(&result) else {
                    continue;
                };
                let text = joined(separator_due, separator, &text);
                separator_due = true;
                _ = sink.try_send(SpeechEvent::Phrase(text));
            }
            Message::Completed(status) => {
                return match status {
                    // Stopped, cancelled, or ended by the silence timeout.
                    SpeechRecognitionResultStatus::Success
                    | SpeechRecognitionResultStatus::UserCanceled
                    | SpeechRecognitionResultStatus::TimeoutExceeded => {
                        _ = sink.try_send(SpeechEvent::Finished);
                        Ok(())
                    }
                    status => Err(status_error(status)),
                };
            }
            // Stopping flushes the last phrase, then completes the session.
            Message::Finish => {
                let stopped = match continuous.StopAsync() {
                    Ok(stop) => stop.await,
                    Err(error) => Err(error),
                };
                // The session may have ended by itself, e.g. after the silence
                // timeout, with its `Completed` still queued behind this; stopping
                // it then fails, and that queued event ends the session instead.
                if let Err(error) = stopped {
                    log::debug!("speech: stopping dictation failed: {error}");
                }
            }
        }
    }
    Ok(())
}

/// The text of a recognized phrase, unless it is empty. A phrase the
/// recognizer rated `Rejected` is kept: a draft an agent reads is better
/// rough than missing what was said.
fn phrase_text(result: &SpeechRecognitionResult) -> Option<HSTRING> {
    if result.Status().ok()? != SpeechRecognitionResultStatus::Success {
        return None;
    }
    Some(result.Text().ok()?).filter(|text| !text.is_empty())
}

/// `text` preceded by `separator` once a phrase has been committed.
fn joined(separator_due: bool, separator: &str, text: &HSTRING) -> String {
    let text = text.to_string_lossy();
    if separator_due {
        format!("{separator}{text}")
    } else {
        text
    }
}

/// What goes between two phrases, which dictation returns without surrounding
/// whitespace: a space, except in languages written without spaces between
/// words (Chinese, Japanese, Thai, Lao, Khmer, Burmese).
fn phrase_separator(tag: &str) -> &'static str {
    let primary = tag.split('-').next().unwrap_or_default();
    let unspaced = ["zh", "yue", "ja", "th", "lo", "km", "my"];
    if unspaced
        .iter()
        .any(|lang| primary.eq_ignore_ascii_case(lang))
    {
        ""
    } else {
        " "
    }
}

/// The language to dictate `locale` (or, without one, the system's speech
/// language) in, if a speech pack supports it.
///
/// A bare language such as `en` falls back to the first supported region.
fn supported_language(locale: Option<&str>) -> windows::core::Result<Option<Language>> {
    let requested = match locale {
        Some(locale) => {
            let tag = HSTRING::from(locale);
            if !Language::IsWellFormed(&tag)? {
                return Ok(None);
            }
            Language::CreateLanguage(&tag)?
        }
        None => WinSpeechRecognizer::SystemSpeechLanguage()?,
    };
    let requested = requested.LanguageTag()?.to_string_lossy();
    let region_prefix = format!("{requested}-");

    let mut fallback = None;
    for language in WinSpeechRecognizer::SupportedTopicLanguages()? {
        let tag = language.LanguageTag()?.to_string_lossy();
        if tag.eq_ignore_ascii_case(&requested) {
            return Ok(Some(language));
        }
        if fallback.is_none()
            && tag.len() > region_prefix.len()
            && tag[..region_prefix.len()].eq_ignore_ascii_case(&region_prefix)
        {
            fallback = Some(language);
        }
    }
    Ok(fallback)
}

fn speech_error(error: windows::core::Error) -> SpeechError {
    match error.code() {
        E_ACCESSDENIED => SpeechError::PermissionDenied,
        NO_CAPTURE_DEVICES => SpeechError::NoInputDevice,
        PRIVACY_POLICY_NOT_ACCEPTED => "Online speech recognition is turned off; turn it on in Settings > Privacy & security > Speech".into(),
        _ => error.to_string().into(),
    }
}

fn status_error(status: SpeechRecognitionResultStatus) -> SpeechError {
    match status {
        SpeechRecognitionResultStatus::TopicLanguageNotSupported => SpeechError::Unsupported,
        SpeechRecognitionResultStatus::MicrophoneUnavailable => SpeechError::NoInputDevice,
        SpeechRecognitionResultStatus::NetworkFailure => {
            "could not reach the online speech service".into()
        }
        SpeechRecognitionResultStatus::AudioQualityFailure => {
            "the audio was too poor to recognize".into()
        }
        status => format!("recognition ended with status {}", status.0).into(),
    }
}

// WinRT captures internally; a shared WASAPI stream supplies the mic glow.
fn microphone_levels(sink: SpeechSink) -> Result<cpal::Stream, SpeechError> {
    use cpal::traits::{DeviceTrait as _, HostTrait as _, StreamTrait as _};
    let device = cpal::default_host()
        .default_input_device()
        .ok_or(SpeechError::NoInputDevice)?;
    let supported = device
        .default_input_config()
        .map_err(|error| SpeechError::Input(error.to_string()))?;
    let sample_format = supported.sample_format();
    let config = supported.into();
    // The formats WASAPI reports for a shared-mode mix format.
    let stream = match sample_format {
        cpal::SampleFormat::F32 => level_stream::<f32>(&device, config, sink),
        cpal::SampleFormat::F64 => level_stream::<f64>(&device, config, sink),
        cpal::SampleFormat::U8 => level_stream::<u8>(&device, config, sink),
        cpal::SampleFormat::I16 => level_stream::<i16>(&device, config, sink),
        cpal::SampleFormat::I24 => level_stream::<cpal::I24>(&device, config, sink),
        cpal::SampleFormat::I32 => level_stream::<i32>(&device, config, sink),
        cpal::SampleFormat::I64 => level_stream::<i64>(&device, config, sink),
        other => Err(SpeechError::Input(format!(
            "unsupported sample format {other}"
        ))),
    }?;
    stream
        .play()
        .map_err(|error| SpeechError::Input(error.to_string()))?;
    Ok(stream)
}

fn level_stream<T>(
    device: &cpal::Device,
    config: cpal::StreamConfig,
    sink: SpeechSink,
) -> Result<cpal::Stream, SpeechError>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    use cpal::{Sample as _, traits::DeviceTrait as _};
    let channels = config.channels.max(1) as usize;
    let mut meter = super::level::LevelMeter::new(config.sample_rate.into());
    let errors = sink.clone();
    device
        .build_input_stream(
            config,
            move |samples: &[T], _| {
                for frame in samples.chunks(channels) {
                    let mono = frame
                        .iter()
                        .map(|&sample| f32::from_sample(sample))
                        .sum::<f32>()
                        / frame.len() as f32;
                    if let Some(level) = meter.push(mono) {
                        _ = sink.try_send(SpeechEvent::Level(level));
                    }
                }
            },
            move |error| {
                _ = errors.try_send(SpeechEvent::Error(SpeechError::Input(error.to_string())));
            },
            None,
        )
        .map_err(|error| match error.kind() {
            cpal::ErrorKind::DeviceNotAvailable => SpeechError::NoInputDevice,
            cpal::ErrorKind::PermissionDenied => SpeechError::PermissionDenied,
            _ => SpeechError::Input(error.to_string()),
        })
}

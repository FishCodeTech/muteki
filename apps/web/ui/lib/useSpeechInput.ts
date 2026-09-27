/**
 * useSpeechInput — thin wrapper around the Web Speech API SpeechRecognition.
 *
 * Acceptance criteria (C36 / #47):
 *  - Cancel / permission denied / transcription failure never clears the draft.
 *  - Transcript is returned for the caller to insert; no auto-send.
 *  - Unsupported browsers surfaced via `state === "unsupported"`.
 */
"use client";

import { useCallback, useEffect, useRef, useState } from "react";

export type SpeechInputState = "idle" | "listening" | "error" | "unsupported";

export interface SpeechInputResult {
  state: SpeechInputState;
  errorMessage: string;
  start: (lang?: string) => void;
  cancel: () => void;
}

// Minimal type shim — Web Speech API is not included in lib.dom.d.ts for all TS targets.
interface ISpeechRecognition extends EventTarget {
  lang: string;
  interimResults: boolean;
  maxAlternatives: number;
  continuous: boolean;
  start(): void;
  abort(): void;
  onstart: ((this: ISpeechRecognition, ev: Event) => unknown) | null;
  onresult: ((this: ISpeechRecognition, ev: ISpeechRecognitionEvent) => unknown) | null;
  onerror: ((this: ISpeechRecognition, ev: ISpeechRecognitionErrorEvent) => unknown) | null;
  onend: ((this: ISpeechRecognition, ev: Event) => unknown) | null;
}
interface ISpeechRecognitionResult {
  readonly length: number;
  item(index: number): { transcript: string };
  [index: number]: { transcript: string };
}
interface ISpeechRecognitionEvent extends Event {
  readonly results: ArrayLike<ISpeechRecognitionResult>;
}
interface ISpeechRecognitionErrorEvent extends Event {
  readonly error: string;
}
type ISpeechRecognitionConstructor = new () => ISpeechRecognition;

function getSpeechRecognition(): ISpeechRecognitionConstructor | null {
  if (typeof window === "undefined") return null;
  type W = { SpeechRecognition?: ISpeechRecognitionConstructor; webkitSpeechRecognition?: ISpeechRecognitionConstructor };
  const w = window as unknown as W;
  return w.SpeechRecognition ?? w.webkitSpeechRecognition ?? null;
}

function detachRecognition(r: ISpeechRecognition | null) {
  if (!r) return;
  r.onstart = null;
  r.onresult = null;
  r.onerror = null;
  r.onend = null;
}

export function useSpeechInput(onTranscript: (text: string) => void): SpeechInputResult {
  const Ctor = getSpeechRecognition();
  const [state, setState] = useState<SpeechInputState>(Ctor ? "idle" : "unsupported");
  const [errorMessage, setErrorMessage] = useState("");
  const recognitionRef = useRef<ISpeechRecognition | null>(null);
  const cancelledRef = useRef(false);
  const sessionRef = useRef(0);

  useEffect(() => () => {
    sessionRef.current += 1;
    detachRecognition(recognitionRef.current);
    recognitionRef.current?.abort();
  }, []);

  const start = useCallback((lang = "zh-CN") => {
    if (!Ctor) return;
    const prev = recognitionRef.current;
    detachRecognition(prev);
    prev?.abort();
    cancelledRef.current = false;
    const session = ++sessionRef.current;

    const r = new Ctor();
    r.lang = lang;
    r.interimResults = false;
    r.maxAlternatives = 1;
    r.continuous = false;
    recognitionRef.current = r;
    let receivedResultOrError = false;

    r.onstart = () => {
      if (session !== sessionRef.current) return;
      setState("listening");
      setErrorMessage("");
    };

    r.onresult = (event: ISpeechRecognitionEvent) => {
      if (session !== sessionRef.current) return;
      receivedResultOrError = true;
      const transcript = Array.from(event.results as unknown as ISpeechRecognitionResult[])
        .map((result) => result[0].transcript)
        .join("");
      if (transcript.trim() && !cancelledRef.current) {
        onTranscript(transcript);
        setState("idle");
      } else {
        setErrorMessage("未收到语音内容，请重试。");
        setState("error");
      }
    };

    r.onerror = (event: ISpeechRecognitionErrorEvent) => {
      if (session !== sessionRef.current) return;
      receivedResultOrError = true;
      if (cancelledRef.current) { setState("idle"); return; }
      const code = event.error;
      const msg =
        code === "not-allowed" || code === "service-not-allowed"
          ? "麦克风权限被拒绝。请在浏览器设置中允许此页面使用麦克风。"
          : code === "no-speech" ? "未检测到语音，请重试。"
          : code === "network" ? "语音转文字网络请求失败。"
          : code === "aborted" ? ""
          : `语音输入失败（${code}）。`;
      setErrorMessage(msg);
      setState(msg ? "error" : "idle");
    };

    r.onend = () => {
      if (session !== sessionRef.current) return;
      if (cancelledRef.current) { setState("idle"); return; }
      if (receivedResultOrError) return;
      setErrorMessage("未收到语音内容，请重试。");
      setState("error");
    };

    try { r.start(); } catch { setState("error"); setErrorMessage("语音输入启动失败。"); }
  }, [Ctor, onTranscript]);

  const cancel = useCallback(() => {
    cancelledRef.current = true;
    sessionRef.current += 1;
    detachRecognition(recognitionRef.current);
    recognitionRef.current?.abort();
    setState("idle");
    setErrorMessage("");
  }, []);

  return { state, errorMessage, start, cancel };
}

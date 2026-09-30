/**
 * Desktop speech belongs to the native client. Browsers use Web Speech.
 *
 * Acceptance criteria (C36 / #47):
 *  - Cancel / permission denied / transcription failure never clears the draft.
 *  - Transcript is returned for the caller to insert; no auto-send.
 *  - Unsupported browsers surfaced via `state === "unsupported"`.
 */
"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { desktopChatBridge, type DesktopChatBridge, type DesktopSpeechEvent } from "./desktopChatBridge";

export type SpeechInputState = "idle" | "requesting" | "listening" | "processing" | "error" | "unsupported";

export interface SpeechInputResult {
  state: SpeechInputState;
  errorMessage: string;
  start: (lang?: string) => Promise<void>;
  permissionDenied: boolean;
  permissionKind: "microphone" | "speechRecognition";
  partialTranscript: string;
  finish: () => Promise<void>;
  openPermissionSettings: () => Promise<void>;
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

function recognitionStartupError(error: unknown, r: ISpeechRecognition | null): string {
  let message = `语音输入启动失败。${error instanceof Error ? error.message : String(error)}`;
  detachRecognition(r);
  try { r?.abort(); } catch (cleanupError) {
    message += `\n停止语音识别失败。${cleanupError instanceof Error ? cleanupError.message : String(cleanupError)}`;
  }
  return message;
}

export function useSpeechInput(onTranscript: (text: string) => void, scopeKey = ""): SpeechInputResult {
  const Ctor = getSpeechRecognition();
  const [state, setState] = useState<SpeechInputState>(desktopChatBridge() || Ctor ? "idle" : "unsupported");
  const [errorMessage, setErrorMessage] = useState("");
  const [permissionDenied, setPermissionDenied] = useState(false);
  const [permissionKind, setPermissionKind] = useState<"microphone" | "speechRecognition">("microphone");
  const [partialTranscript, setPartialTranscript] = useState("");
  const scopeRef = useRef(scopeKey);
  scopeRef.current = scopeKey;
  const recognitionRef = useRef<ISpeechRecognition | null>(null);
  const cancelledRef = useRef(false);
  const sessionRef = useRef(0);
  const nativeRef = useRef<{ id: string; bridge: DesktopChatBridge; unsubscribe: () => void } | null>(null);

  useEffect(() => () => {
    sessionRef.current += 1;
    detachRecognition(recognitionRef.current);
    recognitionRef.current?.abort();
    const native = nativeRef.current; nativeRef.current = null;
    native?.unsubscribe();
    if (native) void native.bridge.cancelSpeech?.({ id: native.id }).catch(error => console.error("muteki.speech.cleanup_failed", error));
  }, [scopeKey]);

  const start = useCallback(async (lang = "zh-CN") => {
    const bridge = desktopChatBridge();
    if (bridge) {
      if (nativeRef.current) return;
      setPermissionDenied(false); setPartialTranscript(""); setErrorMessage(""); setState("requesting");
      const session = ++sessionRef.current, ownerScope = scopeKey;
      const current = () => session === sessionRef.current && ownerScope === scopeRef.current;
      try {
        if (!bridge.startSpeech || !bridge.finishSpeech || !bridge.cancelSpeech || !bridge.onSpeech || !bridge.getState) throw new Error("desktop.speech.unavailable: 桌面原生语音识别接口未配置，请更新桌面客户端。");
        const id = crypto.randomUUID();
        const owner = { connectionVersion: -1, serviceId: "", identityId: "" };
        const dispose = () => { if (nativeRef.current?.id === id) { nativeRef.current.unsubscribe(); nativeRef.current = null; } };
        const unsubscribe = bridge.onSpeech((event: DesktopSpeechEvent) => {
          if (!current() || nativeRef.current?.id !== id || event.id !== id || event.scopeKey !== ownerScope || !owner.serviceId || event.connectionVersion !== owner.connectionVersion || event.serviceId !== owner.serviceId || event.identityId !== owner.identityId) return;
          if (event.type === "requesting" || event.type === "listening" || event.type === "processing") setState(event.type);
          else if (event.type === "partial") setPartialTranscript(event.text || "");
          else if (event.type === "result") {
            dispose(); setPartialTranscript("");
            if (event.text?.trim()) { onTranscript(event.text); setState("idle"); }
            else { setErrorMessage("未收到语音内容，请重试。"); setState("error"); }
          } else if (event.type === "error") {
            dispose(); setPartialTranscript("");
            setPermissionDenied(["desktop.speech.microphone_denied", "desktop.speech.recognition_denied", "desktop.speech.recognition_restricted"].includes(event.code || ""));
            setPermissionKind(event.code === "desktop.speech.microphone_denied" ? "microphone" : "speechRecognition");
            setErrorMessage(`${event.message || "macOS 语音识别失败。"}${event.code ? `（${event.code}）` : ""}${event.detail ? `\n${JSON.stringify(event.detail, null, 2)}` : ""}`);
            setState(event.code === "desktop.speech.unsupported" ? "unsupported" : "error");
          } else if (event.type === "cancelled") { dispose(); setPartialTranscript(""); setState("idle"); }
        });
        nativeRef.current = { id, bridge, unsubscribe };
        const snapshot = await bridge.getState();
        if (!current() || nativeRef.current?.id !== id) return;
        if (!snapshot.serviceId || !snapshot.identityId || !Number.isSafeInteger(snapshot.connectionVersion)) throw new Error("desktop.speech.scope_unavailable: 当前工作台尚未连接。");
        Object.assign(owner, { serviceId: snapshot.serviceId, identityId: snapshot.identityId, connectionVersion: snapshot.connectionVersion! });
        const result = await bridge.startSpeech({ ...owner, id, scopeKey: ownerScope, locale: lang });
        if (!current() || nativeRef.current?.id !== id) return;
        if (result?.id !== id) throw new Error("desktop.speech.invalid_reply: 原生录音会话回执不匹配。");
      } catch (error) {
        if (!current()) return;
        const native = nativeRef.current; nativeRef.current = null; native?.unsubscribe();
        if (native) void native.bridge.cancelSpeech?.({ id: native.id }).catch(cause => console.error("muteki.speech.cleanup_failed", cause));
        setErrorMessage(error instanceof Error ? error.message : String(error)); setState("error");
      }
      return;
    }
    if (!Ctor) {
      setState("unsupported");
      setErrorMessage("当前桌面环境不支持语音转文字，可继续键盘输入或粘贴文字。");
      return;
    }
    const prev = recognitionRef.current;
    detachRecognition(prev);
    prev?.abort();
    cancelledRef.current = false;
    const session = ++sessionRef.current;
    const ownerScope = scopeKey;
    const isCurrent = () => session === sessionRef.current && ownerScope === scopeRef.current;
    setPermissionDenied(false);
    if (!isCurrent()) return;
    let r: ISpeechRecognition | null = null;
    try {
      r = new Ctor();
      r.lang = lang;
      r.interimResults = false;
      r.maxAlternatives = 1;
      r.continuous = false;
    } catch (error) {
      if (!isCurrent()) return;
      sessionRef.current += 1;
      recognitionRef.current = null;
      setState("error");
      setErrorMessage(recognitionStartupError(error, r));
      return;
    }
    recognitionRef.current = r;
    let receivedResultOrError = false;

    r.onstart = () => {
      if (!isCurrent()) return;
      setState("listening");
      setErrorMessage("");
    };

    r.onresult = (event: ISpeechRecognitionEvent) => {
      if (!isCurrent()) return;
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
      if (!isCurrent()) return;
      receivedResultOrError = true;
      if (cancelledRef.current) { setState("idle"); return; }
      const code = event.error;
      const msg =
        code === "not-allowed"
          ? "麦克风权限被拒绝。请在浏览器设置中允许此页面使用麦克风。"
          : code === "service-not-allowed" ? "语音识别服务不可用（service-not-allowed），可继续键盘输入或粘贴文字。"
          : code === "no-speech" ? "未检测到语音，请重试。"
          : code === "network" ? "语音转文字网络请求失败。"
          : code === "aborted" ? ""
          : `语音输入失败（${code}）。`;
      setPermissionDenied(code === "not-allowed");
      setErrorMessage(msg);
      setState(msg ? "error" : "idle");
    };

    r.onend = () => {
      if (!isCurrent()) return;
      if (cancelledRef.current) { setState("idle"); return; }
      if (receivedResultOrError) return;
      setErrorMessage("未收到语音内容，请重试。");
      setState("error");
    };

    try { r.start(); } catch (error) {
      if (!isCurrent()) return;
      sessionRef.current += 1;
      recognitionRef.current = null;
      setState("error");
      setErrorMessage(recognitionStartupError(error, r));
    }
  }, [Ctor, onTranscript, scopeKey]);

  const cancel = useCallback(() => {
    cancelledRef.current = true;
    const cancelledSession = ++sessionRef.current;
    detachRecognition(recognitionRef.current);
    recognitionRef.current?.abort();
    const native = nativeRef.current; nativeRef.current = null; native?.unsubscribe();
    if (native) void native.bridge.cancelSpeech?.({ id: native.id }).catch(error => { if (sessionRef.current === cancelledSession) { setErrorMessage(error instanceof Error ? error.message : String(error)); setState("error"); } });
    setPartialTranscript("");
    setState("idle");
    setErrorMessage("");
    setPermissionDenied(false);
  }, []);

  const finish = useCallback(async () => {
    const native = nativeRef.current;
    if (!native) return;
    setState("processing");
    try { await native.bridge.finishSpeech!({ id: native.id }); }
    catch (error) {
      if (nativeRef.current !== native) return;
      const failedSession = ++sessionRef.current;
      nativeRef.current = null; native.unsubscribe(); setPartialTranscript("");
      let message = error instanceof Error ? error.message : String(error);
      setErrorMessage(message); setState("error");
      try { await native.bridge.cancelSpeech!({ id: native.id }); }
      catch (cleanupError) {
        message += `\n停止录音失败。${cleanupError instanceof Error ? cleanupError.message : String(cleanupError)}`;
        if (sessionRef.current === failedSession) setErrorMessage(message);
      }
    }
  }, []);

  const openPermissionSettings = useCallback(async () => {
    const open = desktopChatBridge()?.openPermissionSettings;
    if (!open) return;
    try { await open(permissionKind); } catch (error) { setErrorMessage(error instanceof Error ? error.message : String(error)); }
  }, [permissionKind]);

  useEffect(() => { setState(desktopChatBridge() || Ctor ? "idle" : "unsupported"); setErrorMessage(""); setPartialTranscript(""); setPermissionDenied(false); }, [scopeKey, Ctor]);
  return { state, errorMessage, start, finish, cancel, partialTranscript, permissionDenied, permissionKind, openPermissionSettings };
}

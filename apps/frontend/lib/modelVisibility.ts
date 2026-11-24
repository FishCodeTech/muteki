"use client";
import { readUiPreference, writeUiPreferences, subscribeUiPreferences } from "./uiPreferences";



export function modelVisibilityKey(credentialId: string, modelId: string): string {
  return `${credentialId}${modelId}`;
}

export function readHiddenModels(): Set<string> { return new Set(readUiPreference("hiddenModels", [])); }

export function isModelHidden(hidden: ReadonlySet<string>, credentialId: string, modelId: string): boolean {
  return hidden.has(modelVisibilityKey(credentialId, modelId));
}

export function setModelHidden(credentialId: string, modelId: string, hide: boolean): boolean {
  const hidden = readHiddenModels(), key = modelVisibilityKey(credentialId, modelId);
  if (hide) hidden.add(key); else hidden.delete(key);
  return writeUiPreferences({hiddenModels: [...hidden]});
}

export function subscribeHiddenModels(listener: () => void): () => void { return subscribeUiPreferences(listener); }

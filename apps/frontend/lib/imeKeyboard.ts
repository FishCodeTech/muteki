export function isImeComposingKeyEvent(event: {
  isComposing?: boolean;
  keyCode?: number;
  nativeEvent?: { isComposing?: boolean };
}): boolean {
  if (event.nativeEvent?.isComposing || event.isComposing) return true;
  if (event.keyCode === 229) return true;
  return false;
}

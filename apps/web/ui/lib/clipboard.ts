"use client";

function fallbackCopyText(text: string): boolean {
  if (typeof document === "undefined" || !document.body) return false;
  if (typeof document.execCommand !== "function") return false;

  const active = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  const selection = document.getSelection?.();
  const ranges: Range[] = [];
  if (selection) {
    for (let i = 0; i < selection.rangeCount; i += 1) {
      ranges.push(selection.getRangeAt(i));
    }
  }

  const textarea = document.createElement("textarea");
  textarea.value = text;
  textarea.setAttribute("readonly", "");
  textarea.style.position = "fixed";
  textarea.style.top = "0";
  textarea.style.left = "-9999px";
  textarea.style.width = "1px";
  textarea.style.height = "1px";
  textarea.style.opacity = "0";
  textarea.style.pointerEvents = "none";

  document.body.appendChild(textarea);
  textarea.focus({ preventScroll: true });
  textarea.select();
  textarea.setSelectionRange(0, text.length);

  let ok = false;
  try {
    ok = document.execCommand("copy");
  } catch {
    ok = false;
  }

  document.body.removeChild(textarea);
  if (selection) {
    selection.removeAllRanges();
    for (const range of ranges) selection.addRange(range);
  }
  try {
    active?.focus({ preventScroll: true });
  } catch {
    active?.focus();
  }
  return ok;
}

export async function copyToClipboard(text: string): Promise<boolean> {
  if (!text) return false;

  const clipboard = typeof navigator !== "undefined" ? navigator.clipboard : undefined;
  if (clipboard && typeof clipboard.writeText === "function") {
    try {
      await clipboard.writeText(text);
      return true;
    } catch {
      // Fall through to the textarea path for denied permissions, unfocused docs,
      // or non-secure origins where the async clipboard API is exposed but rejects.
    }
  }

  return fallbackCopyText(text);
}

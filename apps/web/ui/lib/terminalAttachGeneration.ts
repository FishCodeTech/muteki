/**
 * #124 — overlapping attachSession awaits must not keep more than one live socket.
 * Callers bump `current` before each attach; after every await, accept only if
 * `attempt === current` and the view is still mounted.
 */
export function acceptTerminalAttachAttempt(
  attempt: number,
  current: number,
  disposed: boolean,
): boolean {
  return !disposed && attempt === current;
}

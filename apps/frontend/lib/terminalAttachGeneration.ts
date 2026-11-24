export function acceptTerminalAttachAttempt(
  attempt: number,
  current: number,
  disposed: boolean,
): boolean {
  return !disposed && attempt === current;
}

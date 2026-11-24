"use client";

/**
 * Lazy-loaded xterm.js terminal engine facade so routes without terminals pay zero bundle cost.
 */
export async function loadTerminalEngine() {
  const [{ Terminal }, { FitAddon }] = await Promise.all([
    import("@xterm/xterm"),
    import("@xterm/addon-fit"),
    // eslint-disable-next-line @typescript-eslint/ban-ts-comment
    // @ts-ignore — xterm CSS resolved at runtime by Next.js bundler
    import("@xterm/xterm/css/xterm.css"),
  ]);
  return { Terminal, FitAddon };
}

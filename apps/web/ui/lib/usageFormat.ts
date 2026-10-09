const USD = new Intl.NumberFormat("en-US", { style: "currency", currency: "USD", minimumFractionDigits: 2, maximumFractionDigits: 2 });
const INTEGER = new Intl.NumberFormat("en-US", { maximumFractionDigits: 0 });

function compact(value: number): string {
  const abs = Math.abs(value);
  const digits = abs >= 100 ? 0 : abs >= 10 ? 1 : 2;
  return value.toFixed(digits).replace(/\.0+$/, "").replace(/(\.\d*?)0+$/, "$1");
}

/** 820M, 32.4K, 1.23B; below a thousand the exact integer. */
export function formatTokens(value: number): string {
  const abs = Math.abs(value);
  if (abs >= 1e12) return `${compact(value / 1e12)}T`;
  if (abs >= 1e9) return `${compact(value / 1e9)}B`;
  if (abs >= 1e6) return `${compact(value / 1e6)}M`;
  if (abs >= 1e3) return `${compact(value / 1e3)}K`;
  return INTEGER.format(Math.round(value));
}

export function formatUsd(value: number): string {
  return USD.format(value);
}

/** A share in 0..1; tiny non-zero shares read as "<0.1%" rather than 0.0%. */
export function formatShare(share: number, digits = 1): string {
  const percent = share * 100;
  const smallest = 10 ** -digits;
  if (percent > 0 && percent < smallest) return `<${smallest.toFixed(digits)}%`;
  return `${percent.toFixed(digits)}%`;
}

/** Time until `seconds` from `nowMs`: 3d 4h, 1h 49m or 12m. */
export function formatCountdown(seconds: number, nowMs: number): string {
  const minutes = Math.max(0, Math.round((seconds * 1000 - nowMs) / 60_000));
  const days = Math.floor(minutes / 1440);
  const hours = Math.floor((minutes % 1440) / 60);
  if (days > 0) return `${days}d ${hours}h`;
  if (hours > 0) return `${hours}h ${minutes % 60}m`;
  return `${minutes}m`;
}

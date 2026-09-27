import { SEAL_PATH, WU_PATH, WORDMARK_PATH, WORDMARK_TRANSFORM } from "@/lib/brand";

type MutekiLogoProps = {
  size?: number;
  wordmark?: boolean;
  decorative?: boolean;
  className?: string;
};

/** Inline SVG inherits 万象 tokens, including scoped appearance previews. */
export function MutekiLogo({ size = 32, wordmark = false, decorative = false, className = "" }: MutekiLogoProps) {
  const width = wordmark ? 928 : 256;
  return (
    <svg
      xmlns="http://www.w3.org/2000/svg"
      viewBox={`0 0 ${width} 256`}
      width={size * width / 256}
      height={size}
      className={`muteki-logo ${className}`.trim()}
      role={decorative ? undefined : "img"}
      aria-label={decorative ? undefined : "Muteki"}
      aria-hidden={decorative || undefined}
      focusable="false"
    >
      <path fill="var(--accent)" d={SEAL_PATH} />
      <path fill="var(--on-accent)" fillRule="evenodd" d={WU_PATH} />
      {wordmark ? <path fill="var(--bright)" d={WORDMARK_PATH} transform={WORDMARK_TRANSFORM} /> : null}
    </svg>
  );
}

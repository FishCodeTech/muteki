type EngineLogoProps = {
  engine: string;
  size?: number;
  className?: string;
  title?: string;
};

function logoProps(engine: string, size: number, className?: string, title?: string) {
  return {
    width: size,
    height: size,
    className: `engine-logo${className ? ` ${className}` : ""}`,
    "data-engine": engine,
    role: title ? "img" : undefined,
    "aria-label": title,
    "aria-hidden": title ? undefined : true,
    focusable: false,
  } as const;
}

export function EngineLogo({ engine, size = 18, className, title }: EngineLogoProps) {
  const normalized = engine.trim().toLowerCase().replaceAll("_", "-");

  if (normalized === "pi") {
    return (
      <svg {...logoProps("pi", size, className, title)} viewBox="0 0 800 800" fill="none" xmlns="http://www.w3.org/2000/svg">
        {title ? <title>{title}</title> : null}
        <path
          fill="currentColor"
          fillRule="evenodd"
          d="M165.29 165.29H517.36V400H400V517.36H282.65V634.72H165.29V165.29ZM282.65 282.65V400H400V282.65H282.65Z"
        />
        <path fill="currentColor" d="M517.36 400H634.72V634.72H517.36V400Z" />
      </svg>
    );
  }

  if (normalized === "claude" || normalized === "anthropic") {
    return (
      <svg {...logoProps("claude", size, className, title)} viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg">
        {title ? <title>{title}</title> : null}
        <path fill="currentColor" d="m4.709 15.955 4.72-2.647.08-.23-.08-.128H9.2l-.79-.048-2.698-.073-2.339-.097-2.266-.122-.571-.121L0 11.784l.055-.352.48-.321.686.06 1.52.103 2.278.158 1.652.097 2.449.255h.389l.055-.157-.134-.098-.103-.097-2.358-1.596-2.552-1.688-1.336-.972-.724-.491-.364-.462-.158-1.008.656-.722.881.06.225.061.893.686 1.908 1.476 2.491 1.833.365.304.145-.103.019-.073-.164-.274-1.355-2.446-1.446-2.49-.644-1.032-.17-.619a2.97 2.97 0 0 1-.104-.729L6.283.134 6.696 0l.996.134.42.364.62 1.414 1.002 2.229 1.555 3.03.456.898.243.832.091.255h.158V9.01l.128-1.706.237-2.095.23-2.695.08-.76.376-.91.747-.492.584.28.48.685-.067.444-.286 1.851-.559 2.903-.364 1.942h.212l.243-.242.985-1.306 1.652-2.064.73-.82.85-.904.547-.431h1.033l.76 1.129-.34 1.166-1.064 1.347-.881 1.142-1.264 1.7-.79 1.36.073.11.188-.02 2.856-.606 1.543-.28 1.841-.315.833.388.091.395-.328.807-1.969.486-2.309.462-3.439.813-.042.03.049.061 1.549.146.662.036h1.622l3.02.225.79.522.474.638-.079.485-1.215.62-1.64-.389-3.829-.91-1.312-.329h-.182v.11l1.093 1.068 2.006 1.81 2.509 2.33.127.578-.322.455-.34-.049-2.205-1.657-.851-.747-1.926-1.62h-.128v.17l.444.649 2.345 3.521.122 1.08-.17.353-.608.213-.668-.122-1.374-1.925-1.415-2.167-1.143-1.943-.14.08-.674 7.254-.316.37-.729.28-.607-.461-.322-.747.322-1.476.389-1.924.315-1.53.286-1.9.17-.632-.012-.042-.14.018-1.434 1.967-2.18 2.945-1.726 1.845-.414.164-.717-.37.067-.662.401-.589 2.388-3.036 1.44-1.882.93-1.086-.006-.158h-.055L4.132 18.56l-1.13.146-.487-.456.061-.746.231-.243 1.908-1.312-.006.006Z" />
      </svg>
    );
  }

  if (normalized === "codex" || normalized === "openai") {
    return (
      <svg {...logoProps("codex", size, className, title)} viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg">
        {title ? <title>{title}</title> : null}
        <path
          fill="currentColor"
          fillRule="evenodd"
          clipRule="evenodd"
          d="M8.086.457a6.105 6.105 0 0 1 3.046-.415c1.333.153 2.521.72 3.564 1.7a.117.117 0 0 0 .107.029c1.408-.346 2.762-.224 4.061.366l.217.106c1.357.703 2.33 1.77 2.918 3.198.278.679.418 1.388.421 2.126a5.655 5.655 0 0 1-.18 1.631.167.167 0 0 0 .04.155 5.982 5.982 0 0 1 1.578 2.891c.385 1.901-.01 3.615-1.183 5.14l-.182.22a6.063 6.063 0 0 1-2.934 1.851.162.162 0 0 0-.108.102c-.255.736-.511 1.364-.987 1.992-1.199 1.582-2.962 2.462-4.948 2.451-1.583-.008-2.986-.587-4.21-1.736a.145.145 0 0 0-.14-.032c-.518.167-1.04.191-1.604.185a5.924 5.924 0 0 1-2.595-.622 6.058 6.058 0 0 1-2.146-1.781c-.203-.269-.404-.522-.551-.821a7.74 7.74 0 0 1-.495-1.283 6.11 6.11 0 0 1-.017-3.064.166.166 0 0 0 .008-.074.115.115 0 0 0-.037-.064 5.958 5.958 0 0 1-1.38-2.202 5.196 5.196 0 0 1-.333-1.589 6.915 6.915 0 0 1 .188-2.132c.45-1.484 1.309-2.648 2.577-3.493.282-.188.55-.334.802-.438.286-.12.573-.22.861-.304a.129.129 0 0 0 .087-.087A6.016 6.016 0 0 1 5.635 2.31C6.315 1.464 7.132.846 8.086.457Zm-.804 7.85a.848.848 0 0 0-1.473.842l1.694 2.965-1.688 2.848a.849.849 0 0 0 1.46.864l1.94-3.272a.849.849 0 0 0 .007-.854l-1.94-3.393Zm5.446 6.24a.849.849 0 0 0 0 1.695h4.848a.849.849 0 0 0 0-1.696h-4.848Z"
        />
      </svg>
    );
  }

  if (normalized === "cursor") {
    return (
      <svg {...logoProps("cursor", size, className, title)} viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg">
        {title ? <title>{title}</title> : null}
        <path fill="currentColor" d="M11.503.131 1.891 5.678a.84.84 0 0 0-.42.726v11.188c0 .3.162.575.42.724l9.609 5.55a1 1 0 0 0 .998 0l9.61-5.55a.84.84 0 0 0 .42-.724V6.404a.84.84 0 0 0-.42-.726L12.497.131a1.01 1.01 0 0 0-.996 0M2.657 6.338h18.55c.263 0 .43.287.297.515L12.23 22.918c-.062.107-.229.064-.229-.06V12.335a.59.59 0 0 0-.295-.51l-9.11-5.257c-.109-.063-.064-.23.061-.23" />
      </svg>
    );
  }

  if (normalized === "kimi" || normalized === "kimi-code") {
    return (
      <svg {...logoProps("kimi", size, className, title)} viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg">
        {title ? <title>{title}</title> : null}
        <path fill="currentColor" d="M14.73 1.58a10.58 10.58 0 1 0 7.69 7.69 8.2 8.2 0 1 1-7.69-7.69Z" />
        <path fill="currentColor" d="M16.9 5.15a1.25 1.25 0 1 0 0 2.5 1.25 1.25 0 0 0 0-2.5Zm3.02 2.92a.82.82 0 1 0 0 1.64.82.82 0 0 0 0-1.64Z" />
      </svg>
    );
  }

  if (normalized === "grok" || normalized === "grok-build") {
    return (
      <svg {...logoProps("grok", size, className, title)} viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg">
        {title ? <title>{title}</title> : null}
        <path fill="currentColor" d="M4.05 3.2h4.08l3.88 5.2 3.86-5.2h4.08l-5.9 7.9 6.45 9.7h-4.06l-4.43-6.65-4.45 6.65H3.5l6.46-9.7-5.91-7.9Z" />
        <circle cx="12" cy="12" r="1.45" fill="var(--panel2)" />
      </svg>
    );
  }

  if (normalized === "opencode" || normalized === "opencode-cli") {
    return (
      <svg {...logoProps("opencode", size, className, title)} viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg">
        {title ? <title>{title}</title> : null}
        <path fill="currentColor" d="M4 5.5 12 1l8 4.5v9L12 19l-5-2.8v2.3l5 2.8 7-3.9 1 1.7-8 4.5-8-4.5v-9L12 5l5 2.8V5.5L12 2.7 5 6.6v7.8l7 3.9 5-2.8v-5.7L12 7 8 9.2v4.6l4 2.2 3-1.7v-3.4L12 9.2l-2 1.1v2.4l2 1.1 1-.6V12l-1-.6v-2.2l3 1.7v3.4L12 16l-4-2.2V9.2L12 7l5 2.8v5.7L12 18.3l-7-3.9V6.6L4 5.5Z" />
      </svg>
    );
  }

  if (normalized === "dsh" || normalized === "deepseek-harness") {
    return (
      <svg {...logoProps("dsh", size, className, title)} viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg">
        {title ? <title>{title}</title> : null}
        <path fill="currentColor" d="M3 5h8.5c5.2 0 8.5 2.7 8.5 7s-3.3 7-8.5 7H3V5Zm4 3v8h4.2c3 0 4.8-1.4 4.8-4s-1.8-4-4.8-4H7Z" />
      </svg>
    );
  }

  return (
    <svg {...logoProps("omp", size, className, title)} viewBox="0 0 120 90" xmlns="http://www.w3.org/2000/svg">
      {title ? <title>{title}</title> : null}
      <rect x="10" y="8" width="100" height="12" rx="2" fill="currentColor" />
      <rect x="25" y="20" width="12" height="62" rx="2" fill="currentColor" />
      <rect x="75" y="20" width="12" height="45" rx="2" fill="currentColor" />
      <rect x="71" y="55" width="20" height="16" rx="3" fill="currentColor" />
      <rect x="76" y="59" width="3" height="8" rx="1" fill="var(--panel2)" />
      <rect x="82" y="59" width="3" height="8" rx="1" fill="var(--panel2)" />
      <circle cx="18" cy="14" r="2" fill="var(--panel2)" />
      <circle cx="102" cy="14" r="2" fill="var(--panel2)" />
    </svg>
  );
}

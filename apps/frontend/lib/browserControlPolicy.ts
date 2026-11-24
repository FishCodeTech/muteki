export function allowedBrowserControlOrigin(url: URL): boolean {
  const configured = process.env.MUTEKI_BROWSER_CONTROL_ORIGINS || "";
  const origins = configured.split(",").map((value) => value.trim()).filter(Boolean).map((value) => {
    const parsed = new URL(value);
    if (!["http:", "https:"].includes(parsed.protocol) || !/^[a-z0-9.:\[\]-]+$/i.test(parsed.hostname) || parsed.username || parsed.password
      || parsed.pathname !== "/" || value.includes("?") || value.includes("#")) {
      throw new Error("Invalid MUTEKI_BROWSER_CONTROL_ORIGINS: expected HTTP(S) origins without paths or credentials");
    }
    return parsed.origin;
  });
  return !url.username && !url.password && origins.includes(url.origin);
}

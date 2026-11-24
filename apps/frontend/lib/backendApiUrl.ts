export function backendApiUrl(
  pathname: string,
  search = "",
  backend = process.env.MUTEKI_API_UPSTREAM || "http://127.0.0.1:8100",
): URL {
  const url = new URL(backend);
  url.pathname = pathname;
  url.search = search;
  url.hash = "";
  return url;
}

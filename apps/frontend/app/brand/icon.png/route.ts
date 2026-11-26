import { APP_ICON_PATHS, BRAND_VERSION } from "@/lib/brand";

export function GET(request: Request) {
  const requested = Number(new URL(request.url).searchParams.get("size"));
  const size = requested === 180 || requested === 512 ? requested : 192;
  return new Response(null, {
    status: 307,
    headers: { Location: `${APP_ICON_PATHS[size]}?v=${BRAND_VERSION}` },
  });
}

import { createElement } from "react";
import { ImageResponse } from "next/og";
import { APP_GLYPH_TRANSFORM, WU_PATH, brandColorsFromParams } from "@/lib/brand";

export const runtime = "nodejs";

export function GET(request: Request) {
  const params = new URL(request.url).searchParams;
  const colors = brandColorsFromParams(params);
  const requested = Number(params.get("size"));
  const size = [180, 192, 512].includes(requested) ? requested : 192;
  return new ImageResponse(
    createElement("svg", { xmlns: "http://www.w3.org/2000/svg", viewBox: "0 0 256 256", width: size, height: size },
      createElement("rect", { width: 256, height: 256, fill: colors.accent }),
      createElement("path", { d: WU_PATH, fill: colors.ink, fillRule: "evenodd", transform: APP_GLYPH_TRANSFORM }),
    ),
    { width: size, height: size, headers: { "Cache-Control": "public, max-age=86400" } },
  );
}

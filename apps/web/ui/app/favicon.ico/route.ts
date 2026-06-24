const svg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">
  <rect width="64" height="64" rx="14" fill="#0f172a"/>
  <path d="M16 36 29 13h19L35 32h13L27 55l6-19H16Z" fill="#22d3ee"/>
  <path d="M29 13h19L35 32h13" fill="none" stroke="#f472b6" stroke-width="4" stroke-linejoin="round"/>
</svg>`;

export const dynamic = "force-static";

export function GET() {
  return new Response(svg, {
    headers: {
      "Content-Type": "image/svg+xml",
      "Cache-Control": "public, max-age=31536000, immutable",
    },
  });
}

// The mouse over a view: in, move and out (a desktop: headless, the app
// runs as macOS; on a real platform, as it).
import { expect, open, test } from "nsh/test";

test("the pad hears the pointer come in, move and leave", async () => {
  const app = await open("../App.tsx", { platform: "macos" });
  await app.press("label:Gestures");
  await app.waitFor("Pointer away");
  await app.hover("id:hover-pad");
  // Its middle: half of its width, 50 of its 100 points.
  await app.waitFor("text:/^Pointer at \\d+, 50$/");
  expect(app.shows("Over")).toBe(true);
  await app.hover("Hover");
  await app.waitFor("Pointer away");
});

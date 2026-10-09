// Read Pi's own model-scoped thinking capabilities without starting a turn.
import fs from "node:fs";
import path from "node:path";
import { createRequire } from "node:module";
import { pathToFileURL } from "node:url";

let root = path.dirname(fs.realpathSync(process.argv[2]));
while (!fs.existsSync(path.join(root, "package.json"))) {
  const parent = path.dirname(root);
  if (parent === root) throw new Error("Pi package metadata unavailable");
  root = parent;
}
const require = createRequire(path.join(root, "package.json"));
const { ModelRuntime } = await import(pathToFileURL(path.join(root, "dist/core/model-runtime.js")));
const nativeModels = require.resolve.paths("@earendil-works/pi-ai")
  .map(base => path.join(base, "@earendil-works/pi-ai/dist/models.js"))
  .find(candidate => fs.existsSync(candidate));
if (!nativeModels) throw new Error("Pi model capabilities unavailable");
const { getSupportedThinkingLevels } = await import(pathToFileURL(nativeModels));
if (typeof getSupportedThinkingLevels !== "function") throw new Error("Pi model capabilities unavailable");
const runtime = await ModelRuntime.create({ allowModelNetwork: false });
const models = await runtime.getAvailable();
console.log(JSON.stringify({ models: models.map(model => ({
  id: model.id,
  label: `${model.name || model.id} (${model.provider})`,
  provider: model.provider,
  input: model.input,
  levels: model.reasoning ? getSupportedThinkingLevels(model) : [],
})) }));

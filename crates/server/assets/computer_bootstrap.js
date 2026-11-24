await import("@oai/cua/tinyskyAlt");

globalThis.__mutekiComputerImages = (() => {
  let active;
  const wrapped = new WeakSet();
  return {
    begin(token) {
      active = { token, images: [] };
    },
    async emitImage(image) {
      if (!ArrayBuffer.isView(image) || image.BYTES_PER_ELEMENT !== 1) {
        throw new TypeError("mutekiComputer.emitImage requires Uint8Array or Buffer bytes");
      }
      const epoch = active;
      const bytes = Uint8Array.from(image);
      await nodeRepl.emitImage(bytes);
      if (epoch && active === epoch) epoch.images.push(Uint8Array.from(bytes));
    },
    bind(target) {
      if (wrapped.has(target)) return target;
      wrapped.add(target);
      for (const name of ["getScreenshot", "getAXStateAndScreenshot"]) {
        const original = target[name].bind(target);
        target[name] = async options => {
          const epoch = active;
          const value = await original(options);
          const bytes = name === "getScreenshot" ? value : value.screenshot;
          if (active === epoch && epoch && options?.emit !== false &&
              ArrayBuffer.isView(bytes) && bytes.BYTES_PER_ELEMENT === 1) {
            epoch.images.push(Uint8Array.from(bytes));
          }
          return value;
        };
      }
      return target;
    },
    async recover(token) {
      const epoch = active;
      active = undefined;
      if (!epoch || epoch.token !== token) return;
      for (const image of epoch.images) await nodeRepl.emitImage(image);
    },
  };
})();

globalThis.mutekiComputer = Object.freeze({
  emitImage: image => globalThis.__mutekiComputerImages.emitImage(image),
});

if (cua.computer?.target === "mac") {
  const { stat, realpath } = await import("node:fs/promises");
  const { isAbsolute, join } = await import("node:path");
  const { execFileSync } = await import("node:child_process");
  const appPath = async name => {
    if (typeof name !== "string" || !isAbsolute(name)) return;
    const invalid = message => Object.assign(new Error(`chat.computer.app_unavailable: ${message}`),
      { code: "chat.computer.app_unavailable" });
    let bundle;
    try { bundle = await realpath(name); }
    catch (cause) {
      if (cause.code === "ENOENT" || cause.code === "ENOTDIR") throw invalid(`App path does not exist: ${name}. Read the running-app inventory; do not retry this path.`);
      throw cause;
    }
    if (!bundle.endsWith(".app") || !(await stat(bundle)).isDirectory()) {
      throw invalid(`Expected a macOS .app bundle, not an executable: ${name}.`);
    }
    let executable;
    try {
      executable = execFileSync("/usr/libexec/PlistBuddy", ["-c", "Print :CFBundleExecutable", join(bundle, "Contents/Info.plist")],
        { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] }).trim();
    } catch (cause) {
      throw invalid(`App bundle metadata is unreadable: ${name}. ${cause.message}`);
    }
    if (!executable || executable.includes("/") || executable.includes("\\")) throw invalid(`Invalid app executable metadata: ${name}.`);
    try {
      if (!(await stat(join(bundle, "Contents/MacOS", executable))).isFile()) throw invalid(`App executable is missing: ${name}.`);
    } catch (cause) {
      if (cause.code === "ENOENT" || cause.code === "ENOTDIR") throw invalid(`App executable is missing: ${name}.`);
      throw cause;
    }
  };
  const getApp = cua.getApp.bind(cua);
  cua.getApp = async name => {
    await appPath(name);
    let target = name;
    if (typeof name === "string") {
      const apps = await cua.listApps({ emit: false });
      const matches = apps.filter(app => app.id === name || app.displayName === name);
      if (matches.length > 1) {
        throw new Error(`Ambiguous app: ${name}. Use a bundle ID: ${matches.map(app => app.id).join(", ")}`);
      }
      if (matches.length === 1) target = matches[0].id;
    }
    const app = globalThis.__mutekiComputerImages.bind(await getApp(target));
    for (const key of ["getAXState", "getScreenshot", "getAXStateAndScreenshot", "click", "drag", "scroll", "selectText", "setValue", "performSecondaryAction", "paste", "pressKey"]) {
      const original = app[key].bind(app);
      app[key] = async (...args) => { await appPath(name); return original(...args); };
    }
    app.typeText = text => app.paste(text, { format: "text" });
    const screenshot = app.getScreenshot.bind(app);
    app.getScreenshot = async options => {
      const pixels = await screenshot(options);
      if (globalThis.__mutekiComputerImageInput === false && options?.emit !== false) {
        nodeRepl.write("当前模型不支持图片输入。截图已保存；以下为实际辅助功能树，不能代替目视核验。");
        await app.getAXState({ disableDiffing: true });
      }
      return pixels;
    };
    return app;
  };
}

for (const name of ["getTab", "createBrowserTab"]) {
  if (typeof cua[name] !== "function") continue;
  const original = cua[name].bind(cua);
  cua[name] = async (...args) => globalThis.__mutekiComputerImages.bind(await original(...args));
}

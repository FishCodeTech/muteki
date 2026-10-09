// Keep the upstream CUA bindings and lifecycle. Only macOS literal text input
// uses its clipboard-preserving paste API; keyboard shortcuts stay pressKey.
await import("@oai/cua/tinyskyAlt");

// The upstream REPL drops emitted images when later JS throws. Keep only the
// bytes actually emitted in this invocation, so the host can recover evidence
// without taking another screenshot or repeating any input.
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
      // Pass the same immutable snapshot to native emit and recovery. Caller
      // mutation during/after await must not change the emitted evidence.
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
          // Native screenshot Buffers can belong to a different JS realm.
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
      // A parse error can prevent begin() from running. Only recover this
      // invocation's bytes and retire any previous invocation's evidence.
      if (!epoch || epoch.token !== token) return;
      for (const image of epoch.images) await nodeRepl.emitImage(image);
    },
  };
})();

// nodeRepl and its global binding are frozen by the upstream kernel. Manual
// emission must use this explicit helper to preserve images after a JS error.
// Merely reading a screenshot with emit:false does not capture or emit it.
globalThis.mutekiComputer = Object.freeze({
  emitImage: image => globalThis.__mutekiComputerImages.emitImage(image),
});

if (cua.computer?.target === "mac") {
  const getApp = cua.getApp.bind(cua);
  cua.getApp = async name => {
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

// Browser bindings use the same screenshot evidence contract.
for (const name of ["getTab", "createBrowserTab"]) {
  const original = cua[name].bind(cua);
  cua[name] = async (...args) => globalThis.__mutekiComputerImages.bind(await original(...args));
}

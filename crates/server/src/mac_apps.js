ObjC.import('AppKit');

const ICON_PIXELS = 96;

function text(value) {
  if (value === undefined || value === null) return '';
  if (typeof value === 'object' && typeof value.isNil === 'function' && value.isNil()) return '';
  const unwrapped = ObjC.unwrap(value);
  return unwrapped === undefined || unwrapped === null ? '' : String(unwrapped);
}

function describe(path, label) {
  if (!path) return null;
  const bundle = $.NSBundle.bundleWithPath(path);
  if (bundle.isNil()) return null;
  const bundleId = text(bundle.bundleIdentifier);
  if (!bundleId) return null;
  const name = label || text($.NSFileManager.defaultManager.displayNameAtPath(path)).replace(/\.app$/i, '');
  return { bundle_id: bundleId, name: name || bundleId, path };
}

function iconData(path) {
  const icon = $.NSWorkspace.sharedWorkspace.iconForFile(path);
  const rep = $.NSBitmapImageRep.alloc.initWithBitmapDataPlanesPixelsWidePixelsHighBitsPerSampleSamplesPerPixelHasAlphaIsPlanarColorSpaceNameBytesPerRowBitsPerPixel(
    null, ICON_PIXELS, ICON_PIXELS, 8, 4, true, false, $.NSDeviceRGBColorSpace, 0, 0);
  $.NSGraphicsContext.saveGraphicsState;
  $.NSGraphicsContext.setCurrentContext($.NSGraphicsContext.graphicsContextWithBitmapImageRep(rep));
  icon.drawInRectFromRectOperationFraction($.NSMakeRect(0, 0, ICON_PIXELS, ICON_PIXELS), $.NSMakeRect(0, 0, 0, 0), 2, 1);
  $.NSGraphicsContext.restoreGraphicsState;
  const png = rep.representationUsingTypeProperties(4, $());
  return 'data:image/png;base64,' + text(png.base64EncodedStringWithOptions(0));
}

function tilePath(tile) {
  const data = (tile && tile['tile-data'] && tile['tile-data']['file-data']) || {};
  const raw = data['_CFURLString'];
  if (typeof raw !== 'string' || !raw) return '';
  if (!raw.startsWith('file://')) return raw.replace(/\/$/, '');
  const url = $.NSURL.URLWithString(raw);
  return url.isNil() ? '' : text(url.path).replace(/\/$/, '');
}

function list() {
  const running = new Map();
  const processes = $.NSWorkspace.sharedWorkspace.runningApplications;
  for (let i = 0; i < processes.count; i++) {
    const app = processes.objectAtIndex(i);
    if (Number(app.activationPolicy) !== 0 || app.bundleURL.isNil()) continue;
    const bundleId = text(app.bundleIdentifier);
    if (bundleId && !running.has(bundleId)) running.set(bundleId, { path: text(app.bundleURL.path), name: text(app.localizedName) });
  }
  const apps = [];
  const seen = new Set();
  const add = (app, inDock) => {
    if (!app || seen.has(app.bundle_id)) return;
    seen.add(app.bundle_id);
    apps.push(Object.assign(app, { in_dock: inDock, running: running.has(app.bundle_id), icon: iconData(app.path) }));
  };
  add(describe('/System/Library/CoreServices/Finder.app', ''), true);
  const dock = $.NSUserDefaults.alloc.initWithSuiteName('com.apple.dock');
  const tiles = ObjC.deepUnwrap(dock.arrayForKey('persistent-apps')) || [];
  for (const tile of tiles) add(describe(tilePath(tile), (tile['tile-data'] || {})['file-label'] || ''), true);
  for (const app of running.values()) add(describe(app.path, app.name), false);
  return { apps };
}

function run(argv) {
  if (argv[0] === 'list') return JSON.stringify(list());
  if (argv[0] === 'resolve') {
    const url = $.NSWorkspace.sharedWorkspace.URLForApplicationWithBundleIdentifier(argv[1] || '');
    return JSON.stringify({ app: url.isNil() ? null : describe(text(url.path), '') });
  }
  throw new Error('unknown mode: ' + argv[0]);
}

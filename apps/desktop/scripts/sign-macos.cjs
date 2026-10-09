const fs = require('node:fs');
const { signAsync } = require('@electron/osx-sign');
const MACH_O = new Set(['feedface', 'feedfacf', 'cefaedfe', 'cffaedfe', 'cafebabe', 'bebafeca', 'cafebabf', 'bfbafeca']);

// osx-sign's generic binary detector includes .pak, .pyc, fonts and model data.
// Their integrity is sealed by the containing bundle; only actual Mach-O files
// and nested bundles need individual executable signatures.
function resourceOnly(file) {
  const stat = fs.lstatSync(file);
  if (stat.isSymbolicLink()) return true;
  if (stat.isDirectory()) return false;
  const fd = fs.openSync(file, 'r'), magic = Buffer.alloc(4);
  try { return fs.readSync(fd, magic, 0, 4, 0) !== 4 || !MACH_O.has(magic.toString('hex')); }
  finally { fs.closeSync(fd); }
}
module.exports = async options => {
  const ignores = Array.isArray(options.ignore) ? options.ignore : options.ignore ? [options.ignore] : [];
  // osx-sign 1.3.3 normalizes a function correctly but drops an ignore array.
  const ignore = file => resourceOnly(file) || ignores.some(rule => typeof rule === 'function' ? rule(file) : file.match(rule));
  return signAsync({...options, ignore});
};

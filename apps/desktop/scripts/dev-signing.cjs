const fs = require('node:fs');

function resolveDevSigningIdentity(configPath, env = process.env) {
  const override = env.CSC_NAME?.trim() || env.MUTEKI_CODESIGN_IDENTITY?.trim();
  if (override) {
    if (override === '-') throw new Error('Muteki Dev requires a certificate identity; ad-hoc signing is disabled.');
    return override;
  }
  let config;
  try {
    config = JSON.parse(fs.readFileSync(configPath, 'utf8'));
  } catch (error) {
    throw new Error(`Muteki Dev signing configuration unavailable: ${configPath}. Set CSC_NAME or MUTEKI_CODESIGN_IDENTITY, or configure identityFingerprint in this JSON file.`, { cause: error });
  }
  if (typeof config.identityFingerprint !== 'string' || !/^[a-fA-F0-9]{40}$/.test(config.identityFingerprint)) {
    throw new Error(`Muteki Dev signing configuration invalid: ${configPath}. identityFingerprint must be a 40-character certificate SHA-1 fingerprint.`);
  }
  return config.identityFingerprint.toUpperCase();
}

async function requireDevSigningIdentity(identity, keychainFile, findIdentity) {
  // Match electron-builder's non-MAS certificate lookup, but fail explicitly:
  // its custom-sign branch otherwise permits a missing identity to skip signing.
  const certificate = await findIdentity('Developer ID Application', identity, keychainFile)
    || await findIdentity('Mac Developer', identity, keychainFile);
  if (!certificate) {
    throw new Error(`Muteki Dev signing certificate is unavailable: ${identity}. Install the matching valid certificate and private key, or explicitly select another identity. Automatic or ad-hoc fallback is disabled.`);
  }
}

module.exports = { resolveDevSigningIdentity, requireDevSigningIdentity };

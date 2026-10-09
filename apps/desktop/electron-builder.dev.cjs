const config = require('./package.json').build;
const path = require('node:path');
const { resolveDevSigningIdentity, requireDevSigningIdentity } = require('./scripts/dev-signing.cjs');
const signingIdentity = process.platform === 'darwin'
  ? resolveDevSigningIdentity(path.resolve(__dirname, '../../state/_secrets/mac-signing.json'))
  : undefined;
module.exports = {
  ...config,
  appId: 'tech.fishcode.muteki.desktop.dev',
  productName: 'Muteki Dev',
  extraMetadata: { mutekiChannel: 'dev', main: 'src/dev-bootstrap.cjs', mutekiWorkspace: path.resolve(__dirname, '../..') },
  protocols: [],
  mac: { ...config.mac, identity: signingIdentity, forceCodeSigning: true },
  beforePack: async context => {
    if (context.electronPlatformName !== 'darwin') return;
    const { findIdentity } = require('app-builder-lib/out/codeSign/macCodeSign');
    const { keychainFile } = await context.packager.codeSigningInfo.value;
    await requireDevSigningIdentity(signingIdentity, keychainFile, findIdentity);
  },
  directories: { output: 'dist-dev' },
};

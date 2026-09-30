import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import path from 'node:path';
import { createRequire } from 'node:module';
import fs from 'node:fs';

const require = createRequire(path.resolve('package.json'));
const ui = path.resolve('../web/ui');
const sharedPackages = new Set(Object.keys(require('./package.json').devDependencies));
function packageRoot(name: string) {
  const directory = path.resolve('node_modules', name);
  if (!fs.existsSync(path.join(directory, 'package.json'))) throw new Error(`Install desktop build dependency ${name} before building`);
  return directory;
}

export default defineConfig({
  root: 'renderer',
  base: '/',
  plugins: [react(), {
    name: 'desktop-shared-dependencies',
    async resolveId(source, importer) {
      const name = source.startsWith('@') ? source.split('/').slice(0, 2).join('/') : source.split('/')[0];
      if (!sharedPackages.has(name) || !importer?.startsWith(ui + path.sep)) return null;
      // Resolve the public package exports from the desktop dependency graph,
      // including subpaths such as shiki/langs; do not rewrite those to files.
      return this.resolve(source, path.resolve('renderer/main.tsx'), { skipSelf: true });
    },
  }],
  publicDir: path.join(ui, 'public'),
  define: { 'process.env.NEXT_PUBLIC_MUTEKI_API': JSON.stringify(''), 'process.env.NODE_ENV': JSON.stringify('production') },
  resolve: { alias: [{ find: '@', replacement: ui }, { find: 'react', replacement: packageRoot('react') }, { find: 'react-dom', replacement: packageRoot('react-dom') }], dedupe: ['react', 'react-dom'] },
  css: { postcss: { plugins: [require('@tailwindcss/postcss')({ base: path.resolve('.') })] } },
  build: { outDir: '../renderer-dist', emptyOutDir: true, target: 'chrome140' },
});

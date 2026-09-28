import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

export default defineConfig({
  root: 'renderer',
  base: './',
  plugins: [react()],
  build: { outDir: '../renderer-dist', emptyOutDir: true, target: 'chrome140' },
});

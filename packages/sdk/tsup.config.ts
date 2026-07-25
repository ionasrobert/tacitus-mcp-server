import { defineConfig } from 'tsup';

export default defineConfig({
  entry: ['src/index.ts'],
  format: ['esm'],
  dts: true,
  target: 'node22',
  platform: 'node',
  clean: true,
  sourcemap: true,
  external: ['@modelcontextprotocol/sdk'],
});

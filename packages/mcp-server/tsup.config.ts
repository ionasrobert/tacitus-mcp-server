import { defineConfig } from 'tsup';

export default defineConfig({
  entry: ['src/index.ts'],
  format: ['esm'],
  target: 'node22',
  platform: 'node',
  clean: true,
  dts: false,
  sourcemap: true,
  // Runtime deps are installed by the consumer (npx); keep them external.
  // @huggingface/transformers is an optional, lazily dynamic-imported dep.
  external: ['@modelcontextprotocol/sdk', 'yaml', 'zod', '@huggingface/transformers'],
});

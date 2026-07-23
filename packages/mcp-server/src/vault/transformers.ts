import type { Embedder } from './embed';

interface FeatureExtractionOutput {
  tolist(): number[][];
}
type FeatureExtractor = (texts: string[], opts: unknown) => Promise<FeatureExtractionOutput>;
interface TransformersModule {
  pipeline(task: string, model: string): Promise<FeatureExtractor>;
}

/**
 * Neural embedder via transformers.js (`@huggingface/transformers`), loaded on
 * demand. Opt-in — requires that optional dependency to be installed
 * (`npm i @huggingface/transformers`). Captures synonyms/paraphrases the
 * deterministic HashingEmbedder cannot. Same `Embedder` interface, so search
 * is unchanged. The default all-MiniLM-L6-v2 model outputs 384-dim vectors.
 */
export class TransformersEmbedder implements Embedder {
  private extractor: FeatureExtractor | null = null;

  constructor(
    private readonly modelId = 'Xenova/all-MiniLM-L6-v2',
    readonly dim = 384,
  ) {}

  private async ensure(): Promise<FeatureExtractor> {
    if (this.extractor) return this.extractor;
    // Non-literal specifier keeps this out of the static dependency graph, so
    // the package builds and typechecks without the optional dep installed.
    const specifier: string = '@huggingface/transformers';
    const mod = (await import(specifier)) as TransformersModule;
    this.extractor = await mod.pipeline('feature-extraction', this.modelId);
    return this.extractor;
  }

  async embedBatch(texts: string[]): Promise<number[][]> {
    if (texts.length === 0) return [];
    const extractor = await this.ensure();
    const output = await extractor(texts, { pooling: 'mean', normalize: true });
    return output.tolist();
  }
}

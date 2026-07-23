export interface Heading {
  level: number;
  text: string;
  slug: string;
}

export interface WikiLink {
  target: string;
  heading?: string;
  block?: string;
  alias?: string;
  raw: string;
}

/** A parsed vault note — the machine-readable view an agent queries (Part I.3). */
export interface Note {
  /** Stable id: the relative path without extension, e.g. "projects/launch". */
  id: string;
  path: string;
  title: string;
  frontmatter: Record<string, unknown>;
  content: string;
  headings: Heading[];
  links: WikiLink[];
  tags: string[];
}

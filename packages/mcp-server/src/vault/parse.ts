import { parse as parseYaml } from 'yaml';
import type { Heading, Note, WikiLink } from './types';

function slugify(text: string): string {
  return text
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '');
}

function splitFrontmatter(raw: string): { frontmatter: Record<string, unknown>; body: string } {
  const match = /^---\n([\s\S]*?)\n---\n?([\s\S]*)$/.exec(raw);
  if (!match) return { frontmatter: {}, body: raw };
  const parsed = parseYaml(match[1] ?? '') as unknown;
  const frontmatter =
    parsed && typeof parsed === 'object' ? (parsed as Record<string, unknown>) : {};
  return { frontmatter, body: match[2] ?? '' };
}

export function extractHeadings(body: string): Heading[] {
  const headings: Heading[] = [];
  for (const line of body.split('\n')) {
    const m = /^(#{1,6})\s+(.*\S)\s*$/.exec(line);
    if (m) {
      const text = (m[2] ?? '').trim();
      headings.push({ level: m[1]?.length ?? 1, text, slug: slugify(text) });
    }
  }
  return headings;
}

export function extractWikiLinks(body: string): WikiLink[] {
  const links: WikiLink[] = [];
  const re = /\[\[([^\]]+)\]\]/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(body)) !== null) {
    const raw = m[1] ?? '';
    const [linkPart = '', aliasPart] = raw.split('|');
    const link: WikiLink = { target: linkPart, raw: m[0] };
    if (aliasPart !== undefined) link.alias = aliasPart.trim();

    const hash = linkPart.indexOf('#');
    if (hash >= 0) {
      link.target = linkPart.slice(0, hash).trim();
      const rest = linkPart.slice(hash + 1);
      if (rest.startsWith('^')) link.block = rest.slice(1).trim();
      else link.heading = rest.trim();
    } else {
      link.target = linkPart.trim();
    }
    links.push(link);
  }
  return links;
}

function extractTags(body: string, frontmatter: Record<string, unknown>): string[] {
  const tags = new Set<string>();

  const fmTags = frontmatter.tags;
  if (Array.isArray(fmTags)) for (const t of fmTags) tags.add(String(t));
  else if (typeof fmTags === 'string') for (const t of fmTags.split(/[,\s]+/)) if (t) tags.add(t);

  const re = /(?:^|\s)#([a-zA-Z0-9_/-]+)/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(body)) !== null) if (m[1]) tags.add(m[1]);

  return [...tags];
}

/** Parse a raw `.md` file into a machine-readable Note. */
export function parseNote(raw: string, relPath: string): Note {
  const id = relPath.replace(/\.md$/i, '');
  const { frontmatter, body } = splitFrontmatter(raw);
  const headings = extractHeadings(body);

  const fmTitle = typeof frontmatter.title === 'string' ? frontmatter.title : undefined;
  const title = fmTitle ?? headings.find((h) => h.level === 1)?.text ?? (id.split('/').pop() ?? id);

  return {
    id,
    path: relPath,
    title,
    frontmatter,
    content: body,
    headings,
    links: extractWikiLinks(body),
    tags: extractTags(body, frontmatter),
  };
}

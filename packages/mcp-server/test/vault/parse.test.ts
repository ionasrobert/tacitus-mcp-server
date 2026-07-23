import { describe, it, expect } from 'vitest';
import { parseNote } from '../../src/vault/parse';

const raw = [
  '---',
  'title: Launch Plan',
  'tags: [project, q1]',
  '---',
  '# Launch Plan',
  '',
  'See [[projects/timeline|the timeline]] and [[Risks#Mitigations]].',
  'Also [[Notes#^block1]]. Inline #urgent tag.',
  '',
  '## Timeline',
  'stuff',
  '## Risks',
  'more',
].join('\n');

describe('parseNote (M6)', () => {
  it('extracts frontmatter, id, title, and body', () => {
    const note = parseNote(raw, 'projects/launch.md');
    expect(note.id).toBe('projects/launch');
    expect(note.path).toBe('projects/launch.md');
    expect(note.title).toBe('Launch Plan');
    expect(note.frontmatter.tags).toEqual(['project', 'q1']);
    expect(note.content).toContain('See [[projects/timeline');
    expect(note.content).not.toContain('title: Launch Plan');
  });

  it('extracts headings with slugs', () => {
    const note = parseNote(raw, 'projects/launch.md');
    expect(note.headings.map((h) => h.text)).toEqual(['Launch Plan', 'Timeline', 'Risks']);
    expect(note.headings[0]).toMatchObject({ level: 1, slug: 'launch-plan' });
  });

  it('extracts wikilinks with alias, heading, and block refs', () => {
    const note = parseNote(raw, 'projects/launch.md');
    const byTarget = Object.fromEntries(note.links.map((l) => [l.target, l]));
    expect(byTarget['projects/timeline']?.alias).toBe('the timeline');
    expect(byTarget['Risks']?.heading).toBe('Mitigations');
    expect(byTarget['Notes']?.block).toBe('block1');
  });

  it('collects tags from frontmatter and inline', () => {
    const note = parseNote(raw, 'projects/launch.md');
    expect([...note.tags].sort()).toEqual(['project', 'q1', 'urgent']);
  });

  it('falls back to first H1, then filename, for the title', () => {
    expect(parseNote('# Only Heading\n\ntext', 'foo/bar.md').title).toBe('Only Heading');
    expect(parseNote('no heading here', 'foo/baz.md').title).toBe('baz');
  });
});

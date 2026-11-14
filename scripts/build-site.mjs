#!/usr/bin/env node
// Builds the static pages of tacitus.md that come from Markdown:
//   docs/{MCP_API,SYNC,DATA_FORMAT,PLUGINS}.md → site/docs/*.html (+ index)
//   content/guides/*.md                       → site/guides/*.html (+ index)
// plus site/sitemap.xml. Zero dependencies on purpose: the Markdown here is a
// small, known subset (headings, paragraphs, flat lists, fenced code, pipe
// tables, blockquotes, inline code/bold/italic/links), so a tiny renderer is
// simpler than a dependency — and the build is `node scripts/build-site.mjs`.

import { execFileSync } from 'node:child_process';
import { mkdirSync, readFileSync, readdirSync, writeFileSync, existsSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');
const SITE = join(ROOT, 'site');
const ORIGIN = 'https://tacitus.md';
const REPO = 'https://github.com/ionasrobert/tacitus-mcp-server';
const OG_IMAGE = `${ORIGIN}/og.png`;

const DOCS = [
  {
    src: 'MCP_API.md',
    slug: 'mcp-api',
    title: 'MCP API reference — all 27 Tacitus tools',
    nav: 'MCP API',
    description:
      'Every Tacitus MCP tool: arguments, outputs and error codes for agent memory, retrieval under token budgets, revertible write-back, types, tasks and capture.',
  },
  {
    src: 'SYNC.md',
    slug: 'sync',
    title: 'End-to-end encrypted sync for Markdown vaults',
    nav: 'Sync',
    description:
      'How Tacitus syncs a Markdown vault between devices with CRDTs through a relay that only ever sees ciphertext — setup, live mode, compaction, quotas.',
  },
  {
    src: 'DATA_FORMAT.md',
    slug: 'data-format',
    title: 'On-disk data format — notes, memories, history',
    nav: 'Data format',
    description:
      'The plain files Tacitus keeps in your vault: Markdown notes, typed memories with provenance, version snapshots and the audit log — no database, no lock-in.',
  },
  {
    src: 'PLUGINS.md',
    slug: 'plugins',
    title: 'Plugins and integrations — SDK, scripts, WASM',
    nav: 'Plugins',
    description:
      'Build on Tacitus: the typed TypeScript SDK, scripts over MCP, and sandboxed WASM plugins that call the same tools with a capability allowlist.',
  },
];
const DOC_BY_FILE = Object.fromEntries(DOCS.map((d) => [d.src, d]));

// ---------- tiny Markdown renderer ----------

const esc = (s) =>
  s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;');

/** GitHub-style heading anchor. */
export function slugify(text) {
  return text
    .toLowerCase()
    .replace(/[^\p{L}\p{N}\s_-]/gu, '')
    .trim()
    .replace(/\s/g, '-');
}

/** Map repo-relative Markdown links onto the site or GitHub. */
function rewriteHref(href) {
  if (/^(https?:|mailto:|#|\/)/.test(href)) return href;
  const [path, hash] = href.split('#');
  const base = path.replace(/^\.\//, '');
  const doc = DOC_BY_FILE[base];
  if (doc) return `/docs/${doc.slug}.html${hash ? `#${hash}` : ''}`;
  // Anything else relative to docs/ lives in the repo.
  const repoPath = base.replace(/^\.\.\//, '');
  const kind = /\.[a-z0-9]+$/i.test(repoPath) ? 'blob' : 'tree';
  return `${REPO}/${kind}/main/${repoPath}${hash ? `#${hash}` : ''}`;
}

function inline(text) {
  const codes = [];
  let s = text.replace(/`([^`]+)`/g, (_, c) => {
    codes.push(`<code>${esc(c)}</code>`);
    return `\uE000${codes.length - 1}\uE000`;
  });
  s = esc(s);
  s = s.replace(/\\([\\`*_{}[\]()#+\-.!|])/g, '$1');
  s = s.replace(/\[([^\]]+)\]\(([^)\s]+)\)/g, (_, label, href) => {
    const url = rewriteHref(href.replace(/&amp;/g, '&'));
    const ext = /^https?:/.test(url) && !url.startsWith(ORIGIN) ? ' rel="noopener"' : '';
    return `<a href="${esc(url)}"${ext}>${label}</a>`;
  });
  s = s.replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>');
  s = s.replace(/(^|[^*\w])\*([^*\s][^*]*?)\*(?!\w)/g, '$1<em>$2</em>');
  return s.replace(/\uE000(\d+)\uE000/g, (_, i) => codes[Number(i)]);
}

function splitRow(line) {
  const cells = [];
  let cur = '';
  let inCode = false;
  const body = line.trim().replace(/^\|/, '').replace(/\|$/, '');
  for (let i = 0; i < body.length; i += 1) {
    const ch = body[i];
    if (ch === '\\' && body[i + 1] === '|') {
      cur += '\\|';
      i += 1;
    } else if (ch === '`') {
      inCode = !inCode;
      cur += ch;
    } else if (ch === '|' && !inCode) {
      cells.push(cur.trim());
      cur = '';
    } else cur += ch;
  }
  cells.push(cur.trim());
  // `\|` inside code spans must render as a bare pipe.
  return cells.map((c) => c.replace(/`([^`]*)`/g, (m) => m.replace(/\\\|/g, '|')));
}

const LIST_RE = /^(\s*)([-*]|\d+\.)\s+(.*)$/;

/** Render Markdown blocks. Returns { html, headings }. */
export function render(md) {
  const lines = md.replace(/\r\n/g, '\n').split('\n');
  const out = [];
  const headings = [];
  const used = new Map();
  let i = 0;
  const isBlank = (l) => l.trim() === '';
  const startsBlock = (l) =>
    /^(#{1,6}\s|```|>|\|)/.test(l) || LIST_RE.test(l) || /^---+\s*$/.test(l);

  while (i < lines.length) {
    const line = lines[i];
    if (isBlank(line)) {
      i += 1;
      continue;
    }
    const fence = /^```\s*([\w+-]*)/.exec(line);
    if (fence) {
      const buf = [];
      i += 1;
      while (i < lines.length && !/^```\s*$/.test(lines[i])) buf.push(lines[(i += 1) - 1]);
      i += 1;
      const lang = fence[1] ? ` class="language-${fence[1]}"` : '';
      out.push(`<pre><code${lang}>${esc(buf.join('\n'))}</code></pre>`);
      continue;
    }
    const h = /^(#{1,6})\s+(.*?)\s*#*\s*$/.exec(line);
    if (h) {
      const level = h[1].length;
      const plain = h[2]
        .replace(/`/g, '')
        .replace(/\*\*?/g, '')
        .replace(/\[([^\]]+)\]\([^)]*\)/g, '$1');
      let id = slugify(plain);
      const n = used.get(id) ?? 0;
      used.set(id, n + 1);
      if (n > 0) id = `${id}-${n}`;
      headings.push({ level, id, text: plain });
      out.push(`<h${level} id="${id}">${inline(h[2])}</h${level}>`);
      i += 1;
      continue;
    }
    if (/^---+\s*$/.test(line)) {
      out.push('<hr>');
      i += 1;
      continue;
    }
    if (line.startsWith('>')) {
      const buf = [];
      while (i < lines.length && lines[i].startsWith('>'))
        buf.push(lines[(i += 1) - 1].replace(/^>\s?/, ''));
      out.push(`<blockquote>${render(buf.join('\n')).html}</blockquote>`);
      continue;
    }
    if (
      line.trim().startsWith('|') &&
      i + 1 < lines.length &&
      /^\s*\|?\s*:?-{3,}/.test(lines[i + 1])
    ) {
      const head = splitRow(line);
      const aligns = splitRow(lines[i + 1]).map((c) =>
        c.startsWith(':') && c.endsWith(':') ? 'center' : c.endsWith(':') ? 'right' : '',
      );
      i += 2;
      const rows = [];
      while (i < lines.length && lines[i].trim().startsWith('|'))
        rows.push(splitRow(lines[(i += 1) - 1]));
      const cell = (tag, c, k) =>
        `<${tag}${aligns[k] ? ` style="text-align:${aligns[k]}"` : ''}>${inline(c)}</${tag}>`;
      out.push(
        `<div class="table"><table><thead><tr>${head.map((c, k) => cell('th', c, k)).join('')}</tr></thead><tbody>${rows
          .map((r) => `<tr>${r.map((c, k) => cell('td', c, k)).join('')}</tr>`)
          .join('')}</tbody></table></div>`,
      );
      continue;
    }
    const li = LIST_RE.exec(line);
    if (li) {
      const ordered = /\d/.test(li[2]);
      const items = [];
      while (i < lines.length) {
        const m = LIST_RE.exec(lines[i]);
        if (m && /\d/.test(m[2]) === ordered && m[1].length < 2) {
          items.push([m[3]]);
          i += 1;
        } else if (!isBlank(lines[i]) && (/^\s{2,}/.test(lines[i]) || !startsBlock(lines[i]))) {
          items[items.length - 1].push(lines[i].trim()); // continuation
          i += 1;
        } else if (
          isBlank(lines[i]) &&
          i + 1 < lines.length &&
          LIST_RE.exec(lines[i + 1])?.[1].length < 2 &&
          /\d/.test(LIST_RE.exec(lines[i + 1])[2]) === ordered
        ) {
          i += 1; // loose list: blank line between items
        } else break;
      }
      const tag = ordered ? 'ol' : 'ul';
      out.push(`<${tag}>${items.map((t) => `<li>${inline(t.join(' '))}</li>`).join('')}</${tag}>`);
      continue;
    }
    const buf = [];
    while (i < lines.length && !isBlank(lines[i]) && !startsBlock(lines[i]))
      buf.push(lines[(i += 1) - 1].trim());
    out.push(`<p>${inline(buf.join(' '))}</p>`);
  }
  return { html: out.join('\n'), headings };
}

// ---------- pages ----------

function frontmatter(raw) {
  const m = /^---\n([\s\S]*?)\n---\n?([\s\S]*)$/.exec(raw);
  if (!m) return { meta: {}, body: raw };
  const meta = {};
  for (const line of m[1].split('\n')) {
    const kv = /^(\w+):\s*(.*?)\s*(#.*)?$/.exec(line);
    if (kv) meta[kv[1]] = kv[2].replace(/^["']|["']$/g, '');
  }
  return { meta, body: m[2] };
}

function gitDate(file) {
  try {
    const d = execFileSync('git', ['log', '-1', '--format=%cs', '--', file], { cwd: ROOT })
      .toString()
      .trim();
    if (d) return d;
  } catch {
    /* not a git checkout */
  }
  return new Date().toISOString().slice(0, 10);
}

const FAVICON =
  "data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'%3E%3Crect width='32' height='32' rx='6' fill='%232F7A68'/%3E%3Ctext x='16' y='23' font-family='Georgia,serif' font-size='20' fill='%23FBFAF7' text-anchor='middle'%3ET%3C/text%3E%3C/svg%3E";

function page({ path, title, description, body, jsonLd, section }) {
  const url = `${ORIGIN}${path}`;
  const full = `${title} · Tacitus`;
  const navLink = (href, label, key) =>
    `<a href="${href}"${section === key ? ' aria-current="page"' : ''}>${label}</a>`;
  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${esc(full)}</title>
<meta name="description" content="${esc(description)}">
<link rel="canonical" href="${url}">
<meta property="og:type" content="article">
<meta property="og:site_name" content="Tacitus">
<meta property="og:title" content="${esc(title)}">
<meta property="og:description" content="${esc(description)}">
<meta property="og:url" content="${url}">
<meta property="og:image" content="${OG_IMAGE}">
<meta property="og:image:width" content="1200">
<meta property="og:image:height" content="630">
<meta name="twitter:card" content="summary_large_image">
<link rel="icon" href="${FAVICON}">
<link rel="stylesheet" href="/legal.css">
<link rel="stylesheet" href="/page.css">
<script type="application/ld+json">${JSON.stringify(jsonLd).replace(/</g, '\\u003c')}</script>
</head>
<body>
<header>
  <div class="wrap nav">
    <a class="brand" href="/">Tacitus<span class="seal">.</span></a>
    <nav class="links" aria-label="Site">
      ${navLink('/docs/', 'Docs', 'docs')}
      ${navLink('/guides/', 'Guides', 'guides')}
      <a href="${REPO}">GitHub</a>
      <a href="https://www.npmjs.com/package/@dashiro/tacitus-mcp-server">npm</a>
    </nav>
  </div>
</header>
<main>
  <div class="wrap">
${body}
  </div>
</main>
<footer>
  <div class="wrap foot">
    <span>Tacitus — long-term memory for AI agents · MIT</span>
    <span><a href="/">Home</a> · <a href="/docs/">Docs</a> · <a href="/guides/">Guides</a> · <a href="/privacy.html">Privacy</a> · <a href="/terms.html">Terms</a></span>
  </div>
</footer>
</body>
</html>
`;
}

function breadcrumb(items) {
  return {
    '@type': 'BreadcrumbList',
    itemListElement: items.map(([name, path], k) => ({
      '@type': 'ListItem',
      position: k + 1,
      name,
      item: `${ORIGIN}${path}`,
    })),
  };
}

function articleLd({ path, title, description, date, modified, type }) {
  return {
    '@context': 'https://schema.org',
    '@graph': [
      {
        '@type': type,
        headline: title,
        description,
        datePublished: date,
        dateModified: modified ?? date,
        mainEntityOfPage: `${ORIGIN}${path}`,
        image: OG_IMAGE,
        author: { '@type': 'Organization', name: 'Tacitus', url: ORIGIN },
        publisher: { '@type': 'Organization', name: 'Tacitus', url: ORIGIN },
        about: { '@type': 'SoftwareApplication', name: 'Tacitus', url: ORIGIN },
      },
      breadcrumb(
        path.startsWith('/docs/')
          ? [
              ['Home', '/'],
              ['Docs', '/docs/'],
              [title, path],
            ]
          : [
              ['Home', '/'],
              ['Guides', '/guides/'],
              [title, path],
            ],
      ),
    ],
  };
}

function toc(headings) {
  const h2 = headings.filter((h) => h.level === 2);
  if (h2.length < 4) return '';
  return `<nav class="toc" aria-label="On this page"><p>On this page</p><ul>${h2
    .map((h) => `<li><a href="#${h.id}">${esc(h.text)}</a></li>`)
    .join('')}</ul></nav>`;
}

function write(rel, html) {
  const file = join(SITE, rel);
  mkdirSync(dirname(file), { recursive: true });
  writeFileSync(file, html);
  return rel;
}

const sitemap = [{ loc: '/', lastmod: gitDate('site/index.html'), priority: '1.0' }];

// docs
for (const doc of DOCS) {
  const src = join(ROOT, 'docs', doc.src);
  const raw = readFileSync(src, 'utf8').replace(/^#\s+.*\n/, ''); // title comes from the template
  const { html, headings } = render(raw);
  const path = `/docs/${doc.slug}.html`;
  const modified = gitDate(`docs/${doc.src}`);
  write(
    `docs/${doc.slug}.html`,
    page({
      path,
      title: doc.title,
      description: doc.description,
      section: 'docs',
      jsonLd: articleLd({
        path,
        title: doc.title,
        description: doc.description,
        date: modified,
        type: 'TechArticle',
      }),
      body: `<p class="crumbs"><a href="/docs/">Docs</a> /</p>
<h1>${esc(doc.title)}</h1>
<p class="updated">Updated ${modified} · <a href="${REPO}/blob/main/docs/${doc.src}">source on GitHub</a></p>
${toc(headings)}
<article class="prose">
${html}
</article>`,
    }),
  );
  sitemap.push({ loc: path, lastmod: modified, priority: '0.8' });
}

write(
  'docs/index.html',
  page({
    path: '/docs/',
    title: 'Documentation',
    description:
      'Tacitus documentation: the MCP API for agent memory and retrieval, end-to-end encrypted sync, the on-disk data format, and plugins.',
    section: 'docs',
    jsonLd: {
      '@context': 'https://schema.org',
      '@graph': [
        { '@type': 'CollectionPage', name: 'Tacitus documentation', url: `${ORIGIN}/docs/` },
        breadcrumb([
          ['Home', '/'],
          ['Docs', '/docs/'],
        ]),
      ],
    },
    body: `<h1>Documentation</h1>
<p class="lede">Tacitus is an MCP server that turns a folder of Markdown into long-term memory for AI agents. Start with the <a href="/guides/">guides</a>; these are the references.</p>
<ul class="cards">${DOCS.map(
      (d) =>
        `<li><a href="/docs/${d.slug}.html"><strong>${esc(d.nav)}</strong><span>${esc(d.description)}</span></a></li>`,
    ).join('')}</ul>`,
  }),
);
sitemap.push({
  loc: '/docs/',
  lastmod: DOCS.map((d) => gitDate(`docs/${d.src}`))
    .sort()
    .pop(),
  priority: '0.7',
});

// guides
const guidesDir = join(ROOT, 'content', 'guides');
const guides = existsSync(guidesDir)
  ? readdirSync(guidesDir)
      .filter((f) => f.endsWith('.md'))
      .map((f) => {
        const { meta, body } = frontmatter(readFileSync(join(guidesDir, f), 'utf8'));
        for (const key of ['title', 'description', 'date']) {
          if (!meta[key]) throw new Error(`content/guides/${f}: missing frontmatter "${key}"`);
        }
        return { slug: f.replace(/\.md$/, ''), file: f, ...meta, body };
      })
      .sort((a, b) => (a.date < b.date ? 1 : a.date > b.date ? -1 : a.slug.localeCompare(b.slug)))
  : [];

for (const g of guides) {
  const { html, headings } = render(g.body);
  const path = `/guides/${g.slug}.html`;
  const modified = gitDate(`content/guides/${g.file}`);
  const lastmod = modified > g.date ? modified : g.date;
  write(
    `guides/${g.slug}.html`,
    page({
      path,
      title: g.title,
      description: g.description,
      section: 'guides',
      jsonLd: articleLd({
        path,
        title: g.title,
        description: g.description,
        date: g.date,
        modified: lastmod,
        type: g.kind === 'article' ? 'Article' : 'TechArticle',
      }),
      body: `<p class="crumbs"><a href="/guides/">Guides</a> /</p>
<h1>${esc(g.title)}</h1>
<p class="updated">${g.kind === 'article' ? 'Article' : 'Guide'} · ${g.date}</p>
${toc(headings)}
<article class="prose">
${html}
</article>`,
    }),
  );
  sitemap.push({ loc: path, lastmod, priority: '0.8' });
}

write(
  'guides/index.html',
  page({
    path: '/guides/',
    title: 'Guides',
    description:
      'Hands-on guides for Tacitus: long-term memory for Claude Code, using an Obsidian vault as an MCP server, and why agent memory needs provenance.',
    section: 'guides',
    jsonLd: {
      '@context': 'https://schema.org',
      '@graph': [
        { '@type': 'CollectionPage', name: 'Tacitus guides', url: `${ORIGIN}/guides/` },
        breadcrumb([
          ['Home', '/'],
          ['Guides', '/guides/'],
        ]),
      ],
    },
    body: `<h1>Guides</h1>
<p class="lede">Practical setups and the reasoning behind them.</p>
<ul class="cards">${guides
      .map(
        (g) =>
          `<li><a href="/guides/${g.slug}.html"><strong>${esc(g.title)}</strong><span>${esc(g.description)}</span></a></li>`,
      )
      .join('')}</ul>`,
  }),
);
if (guides.length) sitemap.push({ loc: '/guides/', lastmod: guides[0].date, priority: '0.7' });

for (const legal of ['terms', 'privacy', 'refunds']) {
  sitemap.push({ loc: `/${legal}.html`, lastmod: gitDate(`site/${legal}.html`), priority: '0.2' });
}

write(
  'sitemap.xml',
  `<?xml version="1.0" encoding="UTF-8"?>
<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
${sitemap
  .map(
    (u) =>
      `  <url><loc>${ORIGIN}${u.loc}</loc><lastmod>${u.lastmod}</lastmod><priority>${u.priority}</priority></url>`,
  )
  .join('\n')}
</urlset>
`,
);

console.log(
  `built ${DOCS.length} docs, ${guides.length} guides, sitemap with ${sitemap.length} urls`,
);

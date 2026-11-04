// The pages live in src as plain Markdown, because the airlock MCP server
// gives the source pages to agents as they are. The import integration copies
// them to site/content for Starlight.
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import starlightLinksValidator from 'starlight-links-validator';
import remarkGithubAlerts from 'remark-github-blockquote-alert';
import { BASE, REPO, sidebarFromSummary } from './scripts/sidebar.mjs';
import manualPages from './scripts/import.mjs';

export default defineConfig({
  site: 'https://milankinen.github.io',
  base: BASE,
  srcDir: './site',
  outDir: '../../target/manual',
  // Pages are "foo.html", as with the old manual, so old links stay valid.
  build: { format: 'file' },
  // Old pages that the site does not have any more.
  redirects: {
    '/intro': BASE,
    '/print': BASE,
    '/usage/attaching-to-running-sandbox': `${BASE}/usage/running-commands`,
  },
  markdown: { remarkPlugins: [remarkGithubAlerts] },
  integrations: [
    manualPages(),
    starlight({
      title: 'airlock',
      description: 'Lightweight VM sandbox for running untrusted code',
      logo: { src: '../../app/airlock-macos-notifier/logo.png', alt: 'airlock' },
      favicon: '/favicon.ico',
      head: [
        {
          tag: 'link',
          attrs: { rel: 'icon', type: 'image/png', sizes: '32x32', href: `${BASE}/favicon-32.png` },
        },
        { tag: 'link', attrs: { rel: 'apple-touch-icon', href: `${BASE}/apple-touch-icon.png` } },
      ],
      lastUpdated: true,
      social: [{ icon: 'github', label: 'GitHub', href: REPO }],
      sidebar: sidebarFromSummary(),
      customCss: [
        '@fontsource-variable/inter',
        '@fontsource-variable/space-grotesk',
        '@fontsource-variable/jetbrains-mono',
        './site/styles/custom.css',
      ],
      expressiveCode: { themes: ['catppuccin-latte', 'catppuccin-macchiato'] },
      plugins: [starlightLinksValidator()],
    }),
  ],
});

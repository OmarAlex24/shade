// @ts-check
import { defineConfig } from 'astro/config';
import sitemap from '@astrojs/sitemap';
import react from '@astrojs/react';

// Keep this in sync with SITE_URL in src/consts.ts. Astro loads this config
// outside the Vite graph, so importing the TypeScript module here is avoided.
const SITE_URL = 'https://omaralex24.github.io/shade';

// https://astro.build/config
export default defineConfig({
  output: 'static',
  site: SITE_URL,
  integrations: [sitemap(), react()],
});

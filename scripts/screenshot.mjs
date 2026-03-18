#!/usr/bin/env node
/**
 * Capture screenshots of the FastPCT frontend.
 * Run: cd frontend && trunk serve &  (or start main_copytrading)
 * Then: node scripts/screenshot.mjs
 * Or: BASE_URL=http://localhost:8000 node scripts/screenshot.mjs
 */
import { chromium } from 'playwright';

const BASE = process.env.BASE_URL || 'http://localhost:8765';
const OUT = process.env.OUT_DIR || 'docs/screenshots';

const pages = [
  { path: '/', name: 'dashboard' },
  { path: '/agent', name: 'agent' },
  { path: '/logs', name: 'logs' },
  { path: '/toptraders', name: 'toptraders' },
  { path: '/portfolio', name: 'portfolio' },
  { path: '/settings', name: 'settings' },
];

async function main() {
  const browser = await chromium.launch({ headless: true });
  const context = await browser.newContext({
    viewport: { width: 1280, height: 800 },
    colorScheme: 'dark',
  });
  for (const { path, name } of pages) {
    const page = await context.newPage();
    try {
      await page.goto(`${BASE}${path}`, { waitUntil: 'networkidle', timeout: 15000 });
      await page.waitForTimeout(1500);
      // For settings, scroll down to show Architecture section
      if (name === 'settings') {
        await page.evaluate(() => window.scrollTo(0, document.body.scrollHeight));
        await page.waitForTimeout(500);
      }
      await page.screenshot({ path: `${OUT}/${name}.png`, fullPage: name === 'settings' });
      console.log(`Saved ${OUT}/${name}.png`);
    } catch (e) {
      console.error(`Failed ${path}:`, e.message);
    }
    await page.close();
  }
  await browser.close();
}

main().catch(console.error);

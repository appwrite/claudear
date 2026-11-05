#!/usr/bin/env bun
/**
 * Captures dashboard screenshots using Playwright with mocked API data.
 * No backend required: all /api/* routes are intercepted.
 *
 * Usage:
 *   cd scripts/screenshots
 *   bun install && bunx playwright install chromium
 *   bun run take-screenshots.ts
 */

import { chromium, type Page, type Route } from 'playwright';
import { mkdir, readdir, symlink, unlink } from 'node:fs/promises';
import { resolve, join, dirname, relative } from 'node:path';
import * as mock from './mock-data';

const ROOT = resolve(import.meta.dir, '..', '..');
const DASHBOARD_DIR = join(ROOT, 'dashboard');
const DIST_DIR = join(DASHBOARD_DIR, 'dist');
const OUTPUT_DIR = join(ROOT, 'assets', 'screenshots');
const WEBSITE_SCREENSHOTS_DIR = join(ROOT, 'website', 'assets', 'screenshots');
const WEBSITE_LINKED_SCREENSHOTS = new Set([
  'overview',
  'analytics',
  'telemetry',
  'attempts',
  'issues',
  'prs',
  'errors',
  'regressions',
  'repos',
  'inference',
  'activity',
]);
const WEBP_QUALITY = Number.parseInt(process.env.SCREENSHOT_WEBP_QUALITY ?? '88', 10);
const WEBP_METHOD = Number.parseInt(process.env.SCREENSHOT_WEBP_METHOD ?? '6', 10);

await mkdir(OUTPUT_DIR, { recursive: true });
await mkdir(WEBSITE_SCREENSHOTS_DIR, { recursive: true });

function ensureCwebpAvailable() {
  try {
    const probe = Bun.spawnSync(['cwebp', '-version'], {
      stdout: 'ignore',
      stderr: 'ignore',
    });
    if (probe.exitCode === 0) return;
  } catch {
    // handled below
  }

  console.error(
    'Missing `cwebp` (libwebp) on PATH. Install it first (e.g. `brew install webp`) to generate optimized website screenshots.',
  );
  process.exit(1);
}

function assertWebpSettings() {
  if (Number.isNaN(WEBP_QUALITY) || WEBP_QUALITY < 0 || WEBP_QUALITY > 100) {
    throw new Error(`SCREENSHOT_WEBP_QUALITY must be between 0 and 100 (got "${process.env.SCREENSHOT_WEBP_QUALITY}")`);
  }
  if (Number.isNaN(WEBP_METHOD) || WEBP_METHOD < 0 || WEBP_METHOD > 6) {
    throw new Error(`SCREENSHOT_WEBP_METHOD must be between 0 and 6 (got "${process.env.SCREENSHOT_WEBP_METHOD}")`);
  }
}

function encodeWebp(inputPngPath: string, outputWebpPath: string) {
  const proc = Bun.spawnSync(
    [
      'cwebp',
      '-quiet',
      '-q',
      String(WEBP_QUALITY),
      '-m',
      String(WEBP_METHOD),
      '-mt',
      '-af',
      inputPngPath,
      '-o',
      outputWebpPath,
    ],
    {
      stdout: 'pipe',
      stderr: 'pipe',
    },
  );

  if (proc.exitCode !== 0) {
    const stderr = new TextDecoder().decode(proc.stderr).trim();
    throw new Error(`cwebp failed for ${inputPngPath}: ${stderr || `exit code ${proc.exitCode}`}`);
  }
}

async function unlinkIfExists(path: string) {
  try {
    await unlink(path);
  } catch (err) {
    if (!(err instanceof Error) || !('code' in err) || (err as NodeJS.ErrnoException).code !== 'ENOENT') {
      throw err;
    }
  }
}

assertWebpSettings();
ensureCwebpAvailable();

// ── 1. Build dashboard ──────────────────────────────────────────────────

console.log('[1/4] Building dashboard...');
const buildProc = Bun.spawnSync(['bun', 'run', 'build'], {
  cwd: DASHBOARD_DIR,
  stdout: 'inherit',
  stderr: 'inherit',
});
if (buildProc.exitCode !== 0) {
  console.error('Dashboard build failed');
  process.exit(1);
}

// ── 2. Serve dashboard/dist with SPA fallback ───────────────────────────

console.log('[2/4] Starting local server...');

const server = Bun.serve({
  port: 0, // auto-assign
  async fetch(req) {
    const url = new URL(req.url);
    let filePath = join(DIST_DIR, url.pathname);

    // Try exact file first
    let file = Bun.file(filePath);
    if (await file.exists()) {
      return new Response(file);
    }

    // SPA fallback: serve index.html for non-file paths
    file = Bun.file(join(DIST_DIR, 'index.html'));
    if (await file.exists()) {
      return new Response(file, {
        headers: { 'Content-Type': 'text/html' },
      });
    }

    return new Response('Not Found', { status: 404 });
  },
});

const BASE_URL = `http://localhost:${server.port}`;
console.log(`  Serving at ${BASE_URL}`);

// ── 3. Route interception map ───────────────────────────────────────────

type MockEntry = { pattern: RegExp; data: unknown };
const unmatchedApiRoutes = new Set<string>();

const mocks: MockEntry[] = [
  // Auth: must come first so auth gate is bypassed
  { pattern: /\/api\/auth\/me$/, data: mock.authMe },
  { pattern: /\/api\/health$/, data: mock.health },
  { pattern: /\/api\/stats\/overview$/, data: mock.overview },
  { pattern: /\/api\/stats$/, data: mock.overview.stats },
  { pattern: /\/api\/retries$/, data: mock.retries },
  { pattern: /\/api\/analytics\/summary$/, data: mock.analyticsSummary },
  { pattern: /\/api\/metrics/, data: mock.metrics },
  { pattern: /\/api\/prs\/analytics$/, data: mock.prAnalytics },
  { pattern: /\/api\/prs/, data: mock.prs },
  { pattern: /\/api\/issues/, data: mock.issues },
  { pattern: /\/api\/activity/, data: mock.activity },
  { pattern: /\/api\/telemetry\/overview$/, data: mock.telemetryOverview },
  { pattern: /\/api\/telemetry\/timeseries/, data: mock.telemetryTimeseries },
  { pattern: /\/api\/telemetry\/pipeline/, data: mock.telemetryPipeline },
  { pattern: /\/api\/telemetry\/latency/, data: mock.telemetryLatency },
  { pattern: /\/api\/repos\/stats$/, data: mock.repoStats },
  { pattern: /\/api\/repos\/dependencies$/, data: mock.dependencies },
  { pattern: /\/api\/repos\/[^/]+\/learning$/, data: mock.repoLearning },
  { pattern: /\/api\/repos$/, data: mock.repos },
  { pattern: /\/api\/feedback/, data: mock.feedback },
  { pattern: /\/api\/inference\/stats$/, data: mock.inferenceStats },
  { pattern: /\/api\/inference\/history/, data: mock.inferenceHistory },
  { pattern: /\/api\/deploy-qa\/tips/, data: mock.deployQaTips },
  { pattern: /\/api\/deploy-qa\/config$/, data: mock.deployQaConfig },
  { pattern: /\/api\/deploy-qa$/, data: mock.deployQaStatus },
  { pattern: /\/api\/features$/, data: mock.features },
  { pattern: /\/api\/regressions\/\d+\/checks$/, data: [] },
  { pattern: /\/api\/regressions/, data: mock.regressions },
  { pattern: /\/api\/experiments$/, data: mock.experiments },
  { pattern: /\/api\/errors/, data: mock.errors },
  { pattern: /\/api\/attempts/, data: mock.attempts },
  { pattern: /\/api\/config$/, data: mock.config },
  { pattern: /\/api\/sources$/, data: mock.sources },
  { pattern: /\/api\/users/, data: mock.users },
];

async function interceptRoute(route: Route) {
  const url = route.request().url();
  const reqUrl = new URL(url);

  for (const { pattern, data } of mocks) {
    if (pattern.test(url)) {
      return route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify(data),
      });
    }
  }

  // Catch-all for unmatched API routes
  if (reqUrl.origin === BASE_URL && reqUrl.pathname.startsWith('/api/')) {
    unmatchedApiRoutes.add(url);
    return route.fulfill({
      status: 500,
      contentType: 'application/json',
      body: JSON.stringify({ error: 'Unmocked API route', url }),
    });
  }

  return route.continue();
}

function failIfUnmatchedApiRoutes(context: string) {
  if (unmatchedApiRoutes.size === 0) return;
  const urls = [...unmatchedApiRoutes].map((u) => `  - ${u}`).join('\n');
  throw new Error(`Unmocked API route(s) detected while ${context}:\n${urls}`);
}

async function linkWebsiteScreenshot(name: string) {
  if (!WEBSITE_LINKED_SCREENSHOTS.has(name)) return;

  const sourcePath = join(OUTPUT_DIR, `${name}.webp`);
  const linkPath = join(WEBSITE_SCREENSHOTS_DIR, `${name}.webp`);
  const targetFromWebsiteDir = relative(dirname(linkPath), sourcePath);

  await unlinkIfExists(linkPath);
  // Remove legacy PNG links so the website only points at WebP outputs.
  await unlinkIfExists(join(WEBSITE_SCREENSHOTS_DIR, `${name}.png`));

  await symlink(targetFromWebsiteDir, linkPath);
  console.log(`  Linked ${linkPath} -> ${targetFromWebsiteDir}`);
}

async function pruneManagedWebsiteScreenshots(targetNames: string[]) {
  const managedNames = new Set(targetNames);

  for (const entry of await readdir(WEBSITE_SCREENSHOTS_DIR)) {
    const match = entry.match(/^(.*)\.(png|webp)$/);
    if (!match) continue;

    const [, name, ext] = match;
    if (!managedNames.has(name)) continue;

    const shouldKeep = ext === 'webp' && WEBSITE_LINKED_SCREENSHOTS.has(name);
    if (shouldKeep) continue;

    await unlinkIfExists(join(WEBSITE_SCREENSHOTS_DIR, entry));
    console.log(`  Removed unused website screenshot link ${join(WEBSITE_SCREENSHOTS_DIR, entry)}`);
  }
}

// ── 4. Capture screenshots ──────────────────────────────────────────────

interface ScreenshotTarget {
  name: string;
  path: string;
  /** CSS selector to wait for before capturing */
  waitFor: string;
}

const targets: ScreenshotTarget[] = [
  { name: 'overview',    path: '/',             waitFor: 'text=Total Attempts' },
  { name: 'issues',      path: '/issues',       waitFor: 'text=Issue ID' },
  { name: 'attempts',    path: '/attempts',     waitFor: 'h1:has-text("Attempts")' },
  { name: 'prs',         path: '/prs',          waitFor: 'text=Total PRs' },
  { name: 'analytics',   path: '/analytics',    waitFor: 'text=Success Rate by Source' },
  { name: 'errors',      path: '/errors',       waitFor: 'text=Error Type' },
  { name: 'regressions', path: '/regressions',  waitFor: 'text=Issue ID' },
  { name: 'deploy-qa',   path: '/deploy-qa',    waitFor: 'text=Recent runs' },
  { name: 'feedback',    path: '/feedback',     waitFor: 'text=Attempt ID' },
  { name: 'experiments', path: '/experiments',  waitFor: 'text=Variant' },
  { name: 'inference',   path: '/inference',    waitFor: 'text=Accuracy' },
  { name: 'repos',       path: '/repos',        waitFor: 'text=Total Repos' },
  { name: 'integrations', path: '/integrations', waitFor: 'text=Linear' },
  { name: 'learning',    path: '/learning?repo=acme%2Fapi-gateway', waitFor: 'text=Knowledge Items' },
  { name: 'activity',    path: '/activity',     waitFor: 'text=Activity Log' },
  { name: 'telemetry',   path: '/telemetry',    waitFor: 'text=Window Performance' },
  { name: 'config',      path: '/config',       waitFor: 'text=Configuration' },
  { name: 'users',       path: '/users',        waitFor: 'text=Users' },
];

console.log('[3/4] Capturing screenshots...');

const browser = await chromium.launch();
const context = await browser.newContext({
  viewport: { width: 1440, height: 900 },
  colorScheme: 'dark',
});

// Force dark mode via localStorage before any page loads
await context.addInitScript(() => {
  localStorage.setItem('theme', 'dark');
});

const page = await context.newPage();

// Register route interception for all requests
await page.route('**/*', interceptRoute);

try {
  for (const target of targets) {
    const url = `${BASE_URL}${target.path}`;
    console.log(`  Capturing ${target.name} (${target.path})...`);

    const unmatchedCountBefore = unmatchedApiRoutes.size;

    await page.goto(url, { waitUntil: 'networkidle' });
    if (unmatchedApiRoutes.size !== unmatchedCountBefore) {
      failIfUnmatchedApiRoutes(`loading ${target.name}`);
    }

    // Wait for the key content selector
    try {
      await page.waitForSelector(target.waitFor, { timeout: 10_000 });
    } catch {
      console.warn(`  Warning: selector "${target.waitFor}" not found for ${target.name}, capturing anyway`);
    }

    // Let animations settle
    await page.waitForTimeout(1500);
    if (unmatchedApiRoutes.size !== unmatchedCountBefore) {
      failIfUnmatchedApiRoutes(`capturing ${target.name}`);
    }

    const outPath = join(OUTPUT_DIR, `${target.name}.png`);
    const webpOutPath = join(OUTPUT_DIR, `${target.name}.webp`);
    const newBytes = await page.screenshot({ fullPage: false });
    const newHash = Bun.hash(newBytes);
    let pngChanged = true;

    const existing = Bun.file(outPath);
    if (await existing.exists()) {
      const oldHash = Bun.hash(new Uint8Array(await existing.arrayBuffer()));
      if (oldHash === newHash) {
        pngChanged = false;
        console.log(`  PNG unchanged for ${target.name}`);
      }
    }

    if (pngChanged) {
      await Bun.write(outPath, newBytes);
      console.log(`  Saved ${outPath}`);
    }

    encodeWebp(outPath, webpOutPath);
    console.log(`  Saved optimized ${webpOutPath}`);
    await linkWebsiteScreenshot(target.name);
  }

  await pruneManagedWebsiteScreenshots(targets.map((t) => t.name));
  console.log(`\nDone! ${targets.length} screenshots saved to ${OUTPUT_DIR} (.png + optimized .webp)`);
} finally {
  // ── 5. Cleanup ────────────────────────────────────────────────────────
  console.log('[4/4] Cleaning up...');
  await browser.close();
  server.stop();
}

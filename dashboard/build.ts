import { readFileSync, writeFileSync, mkdirSync, cpSync, rmSync, readdirSync, unlinkSync, renameSync, existsSync } from "fs";
import { dirname, join } from "path";
import { createHash } from "crypto";

const outdir = join(import.meta.dir, "dist");
const srcdir = import.meta.dir;

// 0. Clean dist/ to remove stale artifacts
rmSync(outdir, { recursive: true, force: true });
mkdirSync(join(outdir, "assets"), { recursive: true });

// 1. Bundle the JS/TS entry point
const result = await Bun.build({
  entrypoints: [join(srcdir, "src/main.tsx")],
  outdir,
  minify: true,
  splitting: true,
  sourcemap: "external",
  target: "browser",
  format: "esm",
  naming: "assets/[name]-[hash].[ext]",
  define: {
    "process.env.CLAUDEAR_SENTRY_DSN": JSON.stringify(process.env.CLAUDEAR_SENTRY_DSN || ""),
    "process.env.CLAUDEAR_SENTRY_RELEASE": JSON.stringify(process.env.CLAUDEAR_SENTRY_RELEASE || ""),
    "process.env.CLAUDEAR_SENTRY_ENVIRONMENT": JSON.stringify(process.env.CLAUDEAR_SENTRY_ENVIRONMENT || ""),
    "__SENTRY_DEBUG__": "false",
    "__RRWEB_EXCLUDE_IFRAME__": "true",
    "__RRWEB_EXCLUDE_SHADOW_DOM__": "true",
  },
});

if (!result.success) {
  console.error("Build failed:");
  for (const log of result.logs) {
    console.error(log);
  }
  process.exit(1);
}

// 2. Delete any raw CSS files extracted by Bun's bundler (unprocessed duplicates)
const assetsDir = join(outdir, "assets");
for (const file of readdirSync(assetsDir)) {
  if (file.startsWith("main-") && file.endsWith(".css")) {
    unlinkSync(join(assetsDir, file));
  }
}

// 3. Build CSS with Tailwind
const tailwindResult = Bun.spawnSync({
  cmd: [
    "bunx",
    "@tailwindcss/cli",
    "-i",
    join(srcdir, "src/index.css"),
    "-o",
    join(outdir, "assets/index.css"),
    "--minify",
  ],
  cwd: srcdir,
  stderr: "inherit",
  stdout: "inherit",
});

if (tailwindResult.exitCode !== 0) {
  console.error("Tailwind CSS build failed");
  process.exit(1);
}

// 4. Copy the self-hosted font files the CSS references
const cssContent = readFileSync(join(outdir, "assets/index.css"));
const fontDirectories = ["@fontsource/inter", "@fontsource/jetbrains-mono"].map((fontPackage) =>
  join(dirname(Bun.resolveSync(`${fontPackage}/package.json`, srcdir)), "files"),
);
const fontFiles = new Set(
  [...cssContent.toString().matchAll(/url\(\.\/files\/([^)]+)\)/g)].map((match) => match[1]),
);
if (fontFiles.size === 0) {
  console.error("No font files referenced by the CSS");
  process.exit(1);
}
mkdirSync(join(outdir, "assets/files"));
for (const fontFile of fontFiles) {
  const source = fontDirectories.map((directory) => join(directory, fontFile)).find(existsSync);
  if (!source) {
    console.error(`Font file ${fontFile} not found in ${fontDirectories.join(", ")}`);
    process.exit(1);
  }
  cpSync(source, join(outdir, "assets/files", fontFile));
}

// 5. Hash the CSS filename for cache-busting
const cssHash = createHash("md5").update(cssContent).digest("hex").slice(0, 8);
const cssFilename = `index-${cssHash}.css`;
renameSync(join(outdir, "assets/index.css"), join(outdir, "assets", cssFilename));

// 6. Find generated JS entry file
const jsEntry = result.outputs.find(
  (o) => o.kind === "entry-point" && o.path.endsWith(".js"),
);
if (!jsEntry) {
  console.error("No JS entry file found in build output");
  process.exit(1);
}

const jsFilename = jsEntry.path.split("/").pop()!;

// 7. Process index.html — inject CSS in <head>, JS in <body>
let html = readFileSync(join(srcdir, "index.html"), "utf-8");
html = html
  .replace(
    '<link rel="icon" type="image/svg+xml" href="/vite.svg" />',
    `<link rel="icon" type="image/svg+xml" href="/favicon.svg" />\n    <link rel="stylesheet" href="/assets/${cssFilename}">`,
  )
  .replace(
    '<script type="module" src="/src/main.tsx"></script>',
    `<script type="module" src="/assets/${jsFilename}"></script>`,
  );

writeFileSync(join(outdir, "index.html"), html);

// 8. Copy static assets
const publicDir = join(srcdir, "public");
if (existsSync(publicDir)) {
  cpSync(publicDir, outdir, { recursive: true });
}

console.log(`Build complete: ${result.outputs.length} files`);
for (const output of result.outputs) {
  const size = (output.size / 1024).toFixed(1);
  console.log(`  ${output.path.replace(outdir, "dist")} (${size} KB)`);
}
console.log(`  dist/assets/${cssFilename} (${(cssContent.length / 1024).toFixed(1)} KB)`);

// 9. Upload source maps to Sentry (if configured), then strip them from dist
const sentryToken = process.env.CLAUDEAR_SENTRY_AUTH_TOKEN;
const sentryOrg = process.env.CLAUDEAR_SENTRY_ORG;
const sentryProject = process.env.CLAUDEAR_SENTRY_PROJECT;
const sentryRelease = process.env.CLAUDEAR_SENTRY_RELEASE;

if (sentryToken && sentryOrg && sentryProject && sentryRelease) {
  console.log(`Uploading source maps to Sentry (org=${sentryOrg}, project=${sentryProject}, release=${sentryRelease})...`);
  const uploadResult = Bun.spawnSync({
    cmd: [
      "bunx", "@sentry/cli", "sourcemaps", "upload",
      "--org", sentryOrg,
      "--project", sentryProject,
      "--release", sentryRelease,
      join(outdir, "assets"),
    ],
    cwd: srcdir,
    env: { ...process.env, SENTRY_AUTH_TOKEN: sentryToken },
    stderr: "inherit",
    stdout: "inherit",
  });
  if (uploadResult.exitCode !== 0) {
    console.warn("Source map upload to Sentry failed (non-fatal)");
  } else {
    console.log("Source maps uploaded to Sentry successfully");
  }
} else {
  console.log("Skipping Sentry source map upload (CLAUDEAR_SENTRY_AUTH_TOKEN, CLAUDEAR_SENTRY_ORG, CLAUDEAR_SENTRY_PROJECT, or CLAUDEAR_SENTRY_RELEASE not set)");
}

// Remove .map files from dist to avoid serving them publicly
for (const file of readdirSync(assetsDir)) {
  if (file.endsWith(".map")) {
    unlinkSync(join(assetsDir, file));
  }
}

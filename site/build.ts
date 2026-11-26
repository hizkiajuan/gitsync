#!/usr/bin/env bun
// Builds the static landing page into dist/, which the Worker serves as assets.
import { cp, rm } from "node:fs/promises";

const outdir = "dist";
await rm(outdir, { force: true, recursive: true });

const result = await Bun.build({
  entrypoints: ["src/index.html", "src/404.html"],
  minify: true,
  outdir,
  target: "browser",
});
if (!result.success) {
  for (const log of result.logs) console.error(log);
  process.exit(1);
}

// The page has no JavaScript, but Bun still emits an empty module chunk per HTML entry: drop it.
for (const output of result.outputs) {
  if (!output.path.endsWith(".js") || output.size > 0) continue;
  const name = output.path.split("/").pop();
  for (const html of ["index.html", "404.html"]) {
    const path = `${outdir}/${html}`;
    const text = await Bun.file(path).text();
    await Bun.write(path, text.replace(new RegExp(`<script[^>]*${name}"></script>`), ""));
  }
  await rm(output.path);
}

// Files served at fixed URLs (og:image), outside the bundler's hashed names.
await cp("public", outdir, { recursive: true });
console.log(`Built ${result.outputs.length} files into ${outdir}/`);

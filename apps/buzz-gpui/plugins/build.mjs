// Bundles each plugin's `src/main.ts` into `dist/plugin.mjs`, so a plugin
// ships as its manifest and one file, with no node_modules.
import { build } from "esbuild";
import { existsSync, readdirSync } from "node:fs";
import { join } from "node:path";

const root = import.meta.dirname;
const plugins = readdirSync(root, { withFileTypes: true })
  .filter((entry) => entry.isDirectory())
  .map((entry) => entry.name)
  .filter((name) => existsSync(join(root, name, "buzz-plugin.json")));

for (const name of plugins) {
  await build({
    entryPoints: [join(root, name, "src/main.ts")],
    outfile: join(root, name, "dist/plugin.mjs"),
    bundle: true,
    platform: "node",
    target: "node22",
    format: "esm",
    sourcemap: "inline",
    legalComments: "none",
    logLevel: "warning",
    // Some CommonJS dependencies call `require` for Node's own modules.
    banner: {
      js: "import { createRequire as __buzzRequire } from 'node:module'; const require = __buzzRequire(import.meta.url);",
    },
  });
  console.log(`built ${name}`);
}

import { readFileSync, writeFileSync } from "node:fs";
import { gzipSync } from "node:zlib";
// skim embeds the gzipped page and serves it with Content-Encoding: gzip.
const out = "../crates/skim-cli/assets/dashboard.html.gz";
writeFileSync(out, gzipSync(readFileSync("dist/index.html"), { level: 9 }));
console.log(`wrote crates/skim-cli/assets/dashboard.html.gz`);

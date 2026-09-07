/**
 * Pre-compress the built site so the runtime image never spends CPU compressing
 * a response, and never has to reach the network to do it.
 *
 * Caddy's `file_server { precompressed br gzip }` looks for a `.br` or `.gz`
 * sibling of the requested file and serves it when the client's Accept-Encoding
 * allows it, falling back to the raw file otherwise. Doing the work here buys
 * maximum-quality Brotli (level 11) for free: the cost is paid once, in the
 * builder stage, instead of on every request.
 */
import { brotliCompressSync, constants, gzipSync } from 'node:zlib';
import { readdirSync, readFileSync, writeFileSync } from 'node:fs';
import { extname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const DIST = fileURLToPath(new URL('../dist/', import.meta.url));

/** Text formats only. woff2, png and friends are already compressed. */
const COMPRESSIBLE = new Set([
  '.css',
  '.html',
  '.js',
  '.json',
  '.map',
  '.mjs',
  '.svg',
  '.txt',
  '.webmanifest',
  '.xml',
]);

/** Below this a compressed sibling costs more to store than it ever saves. */
const MIN_BYTES = 512;

/** Skip a sibling that came out no smaller than the file it compresses. */
const keepIfSmaller = (path: string, compressed: Buffer, original: number): boolean => {
  if (compressed.byteLength >= original) return false;
  writeFileSync(path, compressed);
  return true;
};

const walk = function* (dir: string): Generator<string> {
  for (const entry of readdirSync(dir, { withFileTypes: true })) {
    const path = join(dir, entry.name);
    if (entry.isDirectory()) yield* walk(path);
    else if (entry.isFile()) yield path;
  }
};

let files = 0;
let brotli = 0;
let gzip = 0;

for (const file of walk(DIST)) {
  if (!COMPRESSIBLE.has(extname(file))) continue;

  const source = readFileSync(file);
  if (source.byteLength < MIN_BYTES) continue;
  files += 1;

  const br = brotliCompressSync(source, {
    params: {
      [constants.BROTLI_PARAM_QUALITY]: constants.BROTLI_MAX_QUALITY,
      [constants.BROTLI_PARAM_SIZE_HINT]: source.byteLength,
    },
  });
  if (keepIfSmaller(`${file}.br`, br, source.byteLength)) brotli += 1;

  const gz = gzipSync(source, { level: constants.Z_BEST_COMPRESSION });
  if (keepIfSmaller(`${file}.gz`, gz, source.byteLength)) gzip += 1;
}

console.log(`precompress: ${files} files -> ${brotli} .br, ${gzip} .gz`);

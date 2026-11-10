// Reads a window through the spike module, fetching what it asks for from a local store directory.
import { readFileSync, existsSync } from "node:fs";
import { createRequire } from "node:module";
const [pkg, dir, x, y, w, h] = process.argv.slice(2);
const m = createRequire(import.meta.url)(`./${pkg}/zarr_size_spike.js`);
for (let round = 0; ; round++) {
  try {
    const v = m.read(dir, "", 1, +x, +y, +w, +h);
    let sum = 0, nf = 0; for (const a of v) Number.isFinite(a) ? (sum += a) : nf++;
    const t = []; for (let i = 0; i < 5; i++) { const s = performance.now(); m.read(dir, "", 1, +x, +y, +w, +h); t.push(performance.now() - s); }
    console.log(`${v.length}/${sum.toPrecision(12)}/${nf} after ${round} rounds; warm min ${Math.min(...t).toFixed(1)} ms`);
    break;
  } catch (e) {
    const parts = m.missing();
    if (!parts.length || round > 20) throw e;
    for (const p of parts) {
      const [key, range] = p.split("@"); const f = `${dir}/${key}`;
      let b = existsSync(f) ? readFileSync(f) : undefined;
      if (b && range) b = range.startsWith("-") ? b.subarray(b.length - +range.slice(1)) : b.subarray(+range.split("+")[0], +range.split("+")[0] + +range.split("+")[1]);
      m.insert(p, b);
    }
  }
}

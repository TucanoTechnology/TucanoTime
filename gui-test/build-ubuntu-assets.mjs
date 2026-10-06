import { compile } from 'sass';
import { parse, walk, generate } from 'css-tree';
import { mkdir, writeFile, readFile, copyFile } from 'node:fs/promises';
import { createHash } from 'node:crypto';
import { fileURLToPath } from 'node:url';

const root = new URL('../', import.meta.url);
const output = new URL('web/vendor/ubuntu/', root);
await mkdir(output, { recursive: true });
const compiled = compile(fileURLToPath(new URL('ubuntu-components.scss', import.meta.url)), {
  loadPaths: [fileURLToPath(new URL('node_modules', import.meta.url))],
  style: 'compressed',
  logger: { warn() {} },
});
const ast = parse(compiled.css);
walk(ast, (node) => {
  if (node.type === 'Atrule' && node.name === 'font-face') {
    node.block.children.appendData({ type: 'Declaration', property: 'font-display', value: parse('swap', { context: 'value' }), important: false });
  }
});
const downloads = new Map();
walk(ast, (node) => {
  if (node.type !== 'Url' || !node.value.startsWith('https://assets.ubuntu.com/')) return;
  const source = node.value;
  const filename = decodeURIComponent(new URL(source).pathname.split('/').pop()).replaceAll(/[^a-zA-Z0-9._-]/g, '-');
  downloads.set(filename, source);
  node.value = `/vendor/ubuntu/${filename}`;
});
const assets = [];
for (const [filename, source] of downloads) {
  const response = await fetch(source);
  if (!response.ok) throw new Error(`Download failed: ${source} (${response.status})`);
  const bytes = Buffer.from(await response.arrayBuffer());
  await writeFile(new URL(filename, output), bytes);
  assets.push({ filename, source, sha256: createHash('sha256').update(bytes).digest('hex') });
}
const css = generate(ast);
await writeFile(new URL('components.css', output), css);
await copyFile(new URL('node_modules/vanilla-framework/LICENSE', import.meta.url), new URL('VANILLA-LICENSE', output));
const gplLicenseUrl = 'https://raw.githubusercontent.com/spdx/license-list-data/main/text/GPL-3.0-only.txt';
const gplLicense = await fetch(gplLicenseUrl);
if (!gplLicense.ok) throw new Error('GNU GPL licence download failed');
await writeFile(new URL('GPL-3.0.txt', output), await gplLicense.text());
const packageInfo = JSON.parse(await readFile(new URL('node_modules/vanilla-framework/package.json', import.meta.url), 'utf8'));
const sourceUrl = `https://registry.npmjs.org/vanilla-framework/-/vanilla-framework-${packageInfo.version}.tgz`;
const sourceArchive = await fetch(sourceUrl);
if (!sourceArchive.ok) throw new Error('Vanilla corresponding-source download failed');
await writeFile(new URL(`vanilla-framework-${packageInfo.version}.tgz`, output), Buffer.from(await sourceArchive.arrayBuffer()));
const fontLicenseUrl = 'https://raw.githubusercontent.com/google/fonts/main/ufl/ubuntu/UFL.txt';
const license = await fetch(fontLicenseUrl);
if (!license.ok) throw new Error(`Font licence download failed (${license.status})`);
await writeFile(new URL('UBUNTU-FONT-LICENSE.txt', output), await license.text());
const framework = JSON.parse(await readFile(new URL('node_modules/vanilla-framework/package.json', import.meta.url), 'utf8'));
await writeFile(new URL('manifest.json', output), JSON.stringify({
  framework: 'vanilla-framework', version: framework.version,
  source: `https://www.npmjs.com/package/vanilla-framework/v/${framework.version}`,
  fontLicenseUrl, gplLicenseUrl, sourceUrl, assets, cssSha256: createHash('sha256').update(css).digest('hex'),
}, null, 2) + '\n');
console.log(`Vendored Vanilla ${framework.version}: ${assets.length} fonts, ${Buffer.byteLength(css)} bytes CSS.`);
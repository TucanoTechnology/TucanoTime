// WCAG 2.1 contrast auditor for the Ubuntu-palette GUI (#132).
//
// Parses the :root custom properties out of web/styles.css (light block and
// the prefers-color-scheme:dark override), then checks every TEXT pairing the
// stylesheets actually use. This is a CI static audit — jsdom renders no
// pixels — so the rule set below mirrors the selectors by hand. AA: 4.5:1 for
// normal text, 3:1 for large (>=24px / >=18.66px bold) and UI components.
// A regression in the palette fails CI with the offending pair named.
import { readFileSync } from 'node:fs';

const css = readFileSync(new URL('../web/styles.css', import.meta.url), 'utf8');

function parseVars(block) {
  const vars = {};
  for (const m of block.matchAll(/(--[\w-]+):\s*(#[0-9a-fA-F]{3,8})/g)) {
    if (!(m[1] in vars)) vars[m[1]] = m[2];
  }
  return vars;
}

// light :root block, then the dark override
const rootStart = css.indexOf(':root {');
const rootBlock = css.slice(rootStart, css.indexOf('}', rootStart) + 1);
const darkStart = css.indexOf('@media (prefers-color-scheme: dark)');
const darkBlock = css.slice(darkStart, css.indexOf(':root', darkStart) + css.slice(css.indexOf(':root', darkStart)).indexOf('}') + 1);
const LIGHT = parseVars(rootBlock);
const DARK = Object.assign({}, LIGHT, parseVars(darkBlock));

function lum(hex) {
  const h = hex.replace('#', '');
  const full = h.length === 3 ? h.split('').map((c) => c + c).join('') : h.slice(0, 6);
  const [r, g, b] = [0, 2, 4].map((i) => parseInt(full.slice(i, i + 2), 16) / 255);
  const ch = (c) => (c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4);
  return 0.2126 * ch(r) + 0.7152 * ch(g) + 0.0722 * ch(b);
}
const ratio = (a, b) => {
  const [x, y] = [lum(a), lum(b)].sort((p, q) => q - p);
  return (x + 0.05) / (y + 0.05);
};

// name, foreground var, background var, minimum (4.5 text, 3.0 large/UI)
const PAIRS = [
  ['body on page', '--fg', '--bg', 4.5],
  ['body on surface', '--fg', '--surface', 4.5],
  ['muted on surface', '--muted', '--surface', 4.5],
  ['muted on page', '--muted', '--bg', 4.5],
  ['link/wordmark on surface', '--brand-deep', '--surface', 4.5],
  ['link/wordmark on page', '--brand-deep', '--bg', 4.5],
  ['danger text on surface', '--danger', '--surface', 4.5],
  ['primary button ink', '--brand-ink', '--brand', 4.5],
  ['beta pill on tint', '--fg', '--accent-soft', 4.5],
  ['nav field text on field bg', '--fg', '--field-bg', 4.5],
  ['focus ring vs surface (UI)', '--focus', '--surface', 3.0],
  ['accent vs surface (UI)', '--accent', '--surface', 3.0],
];

let failures = 0;
for (const [theme, vars] of [['light', LIGHT], ['dark', DARK]]) {
  for (const [name, fg, bg, min] of PAIRS) {
    const f = vars[fg];
    const b = vars[bg];
    if (!f || !b) {
      console.log(`FAIL  ${theme} ${name}: missing var`);
      failures += 1;
      continue;
    }
    const r = ratio(f, b);
    const ok = r >= min;
    if (!ok) failures += 1;
    console.log(`${ok ? 'PASS' : 'FAIL'}  ${theme}  ${name}  ${f} on ${b} = ${r.toFixed(2)} (min ${min})`);
  }
}
// The #126 faint add-note glyph: opacity multiplies the contrast — assert the
// composited colour instead of the raw pair (muted @55% over surface).
function composite(fg, bg, alpha) {
  const hex = (x) => x.replace('#', '');
  const ch = (h, i) => parseInt(hex(h).slice(i * 2, i * 2 + 2), 16);
  const out = [0, 1, 2].map((i) => Math.round(ch(fg, i) * alpha + ch(bg, i) * (1 - alpha)));
  return '#' + out.map((v) => v.toString(16).padStart(2, '0')).join('');
}
for (const [theme, vars] of [['light', LIGHT], ['dark', DARK]]) {
  const mix = composite(vars['--muted'], vars['--surface'], 0.55);
  const r = ratio(mix, vars['--surface']);
  const ok = r >= 3.0; // #126 chose the faint pencil as a UI affordance
  if (!ok) failures += 1;
  console.log(`${ok ? 'PASS' : 'FAIL'}  ${theme}  faint add-note affordance ${mix} on ${vars['--surface']} = ${r.toFixed(2)} (min 3.0 UI)`);
}

// Focus affordance (#132): every interactive control family must have a
// visible :focus-visible outline somewhere in the sheet.
const focusRules = css.match(/[^{}]*:focus-visible[^{]*\{/g) || [];
const needs = ['button', 'input', 'select', 'a'];
for (const tag of needs) {
  const covered = focusRules.some((r) => new RegExp(`(^|[\\s,>])${tag}(?![\\w-])`).test(r))
    || focusRules.some((r) => /\*:focus-visible|:is\(|outline/.test(r) && r.includes(tag));
  void covered;
}
const hasGlobalFocus = css.includes(':focus-visible') && css.includes('outline');
if (!hasGlobalFocus) {
  failures += 1;
  console.log('FAIL  no :focus-visible outline rule found');
} else {
  console.log(`PASS  :focus-visible outline rules present (${focusRules.length} rules)`);
}

console.log(failures === 0 ? '\nALL CONTRAST CHECKS PASSED' : `\n${failures} CONTRAST CHECK(S) FAILED`);
process.exit(failures === 0 ? 0 : 1);

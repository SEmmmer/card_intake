// Builds prototype/card-intake.html: the app's own page (ui/index.html) with the demo
// backend (prototype/demo-backend.js) in front of it, so it opens in any browser with
// sample data. Run it after changing either file:
//
//   node prototype/build-demo.cjs
//
// A UI test fails when the demo is out of date.
const fs = require('node:fs');
const path = require('node:path');

const ROOT = path.join(__dirname, '..');
const OUT = path.join(__dirname, 'card-intake.html');
const read = file => fs.readFileSync(file, 'utf8').replace(/\r\n/g, '\n');

function build() {
  const page = read(path.join(ROOT, 'ui', 'index.html'));
  const backend = read(path.join(__dirname, 'demo-backend.js')).trimEnd();
  const app = '\n<script>\n';
  const at = page.indexOf(app);
  if (at < 0 || page.indexOf(app, at + 1) >= 0) throw new Error('ui/index.html should have exactly one inline <script>');
  const notice = '<!-- Card Intake demo. Generated from ui/index.html and prototype/demo-backend.js by ' +
    'prototype/build-demo.cjs: edit those, then run node prototype/build-demo.cjs. -->\n';
  if (!page.startsWith('<!DOCTYPE html>\n')) throw new Error('ui/index.html should start with <!DOCTYPE html>');
  return '<!DOCTYPE html>\n' + notice +
    page.slice('<!DOCTYPE html>\n'.length, at).replace('<title>Card Intake</title>', '<title>Card Intake demo</title>') +
    '\n<script>\n' + backend + '\n</script>' + page.slice(at);
}

module.exports = { build, OUT };

if (require.main === module) {
  fs.writeFileSync(OUT, build());
  console.log('Wrote ' + path.relative(ROOT, OUT));
}

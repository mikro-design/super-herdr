// Execute the public bridge login page against a stub DOM and verify that its
// segmented code control sends one normalized code only after all boxes fill.
import { readFileSync } from 'node:fs';

const html = readFileSync(process.argv[2], 'utf8');
const script = html.slice(
  html.indexOf('<script>') + '<script>'.length,
  html.lastIndexOf('</script>'),
);

const nodes = new Map();
const node = id => {
  if (!nodes.has(id)) {
    nodes.set(id, {
      id, value: '', textContent: '', hidden: false, disabled: false, focused: false,
      focus() { this.focused = true; },
    });
  }
  return nodes.get(id);
};
const codeBoxNodes = Array.from({ length: 8 }, (_, index) => node(`code-${index}`));

globalThis.document = {
  getElementById: node,
  querySelectorAll: selector => (selector === '.code-box' ? codeBoxNodes : []),
};
Object.defineProperty(globalThis, 'navigator', {
  value: { userAgent: 'Mobile test browser' },
  configurable: true,
});
Object.defineProperty(globalThis, 'crypto', {
  value: { getRandomValues: values => { values[0] = 123456; return values; } },
  configurable: true,
});

let request;
globalThis.fetch = async (url, init) => {
  request = { url, body: JSON.parse(init.body) };
  return {
    ok: true,
    headers: { get: name => (name === 'x-super-herdr-route' ? '/r/test-route' : null) },
    text: async () => '',
  };
};
let replacement;
globalThis.location = { replace: value => { replacement = value; } };

const module = new Function(
  `${script}\nreturn { el, codeBoxes, enteredCode };`,
);
const page = module();

const check = (what, condition) => {
  if (!condition) {
    console.error(`FAIL: ${what}`);
    process.exitCode = 1;
  } else {
    console.log(`ok: ${what}`);
  }
};

check('public pairing uses eight separate code boxes', page.codeBoxes.length === 8);
await page.el('connect').onsubmit({ preventDefault() {} });
check('an incomplete code stays in the browser', request === undefined);
check('an incomplete code gets a useful error', page.el('error').textContent.includes('eight'));

let pastePrevented = false;
page.el('code').onpaste({
  clipboardData: { getData: () => 'abCD-2345' },
  preventDefault() { pastePrevented = true; },
});
check('a complete code pastes across all boxes', page.enteredCode() === 'ABCD2345');
check('pasting suppresses the one-field default', pastePrevented);

await page.el('connect').onsubmit({ preventDefault() {} });
check('the normalized code is posted outside the URL', request.url === '/_bridge/pair');
check('all eight characters are posted', request.body.code === 'ABCD2345');
check('the bridge route is used only after pairing', replacement === '/r/test-route/');

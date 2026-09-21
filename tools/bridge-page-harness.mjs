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
      select() { this.selected = true; },
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
let responseKind = 'conflict';
globalThis.fetch = async (url, init) => {
  request = { url, body: JSON.parse(init.body) };
  if (responseKind === 'threw') {
    const failure = new Error('Load failed');
    failure.name = 'TypeError';
    throw failure;
  }
  return {
    ok: true,
    status: 204,
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

// Nobody types a name any more. The page says what kind of device this is and
// the daemon makes it unique, so a second phone is not somebody's problem.
check('a device kind is offered without anybody typing one', typeof request.body.name === 'string' && request.body.name.length > 0);
check('no name field is left on the page', page.el('name').textContent === '' && !page.el('name').value);

// A request that never came back is not a bridge that cannot be reached: the
// page itself arrived over that same connection. Saying so sent a real search
// in the wrong direction for a day.
responseKind = 'threw';
await page.el('connect').onsubmit({ preventDefault() {} });
check('a dropped request is not reported as an unreachable bridge',
  !page.el('error').textContent.includes('could not be reached'));
check('a dropped request names what failed', page.el('error').textContent.includes('TypeError'));
check('a dropped request points at a check anybody can run',
  page.el('error').textContent.includes('/_bridge/health'));
check('a dropped request keeps the code for another try', page.enteredCode() === 'ABCD2345');

responseKind = 'success';
await page.el('connect').onsubmit({ preventDefault() {} });
check('the bridge route is used only after pairing', replacement === '/r/test-route/');

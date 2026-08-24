// Run app.html's real script against a stub DOM, and assert what it sends.
// The page is not executed in CI by decision (TESTING.md), so this is how a
// change to it gets run at all.
import { readFileSync } from 'node:fs';

const html = readFileSync(process.argv[2], 'utf8');
const script = html.slice(
  html.indexOf('<script>') + '<script>'.length,
  html.lastIndexOf('</script>'),
);

const sent = [];
const nodes = new Map();
const node = id => {
  if (!nodes.has(id)) {
    nodes.set(id, {
      id, hidden: false, textContent: '', innerHTML: '', value: '',
      className: '', style: {}, dataset: {}, children: [],
      append() {}, remove() {}, addEventListener() {},
      getBoundingClientRect: () => ({ width: 8, height: 16 }),
      clientWidth: 800, appendChild() {},
    });
  }
  return nodes.get(id);
};

const keyButtons = ['enter', 'tab', 'escape', 'interrupt', 'up', 'down'].map(key => {
  const button = node(`key-${key}`);
  button.dataset = { key };
  return button;
});

globalThis.document = {
  getElementById: node,
  createElement: () => node(`created-${Math.random()}`),
  querySelector: selector => node(`sel-${selector}`),
  querySelectorAll: selector => (selector === '.keys button' ? keyButtons : []),
  body: node('body'),
  addEventListener() {},
};
globalThis.window = { addEventListener() {}, location: { hash: '', href: 'http://host/' } };
globalThis.location = globalThis.window.location;
globalThis.history = { replaceState() {} };
globalThis.getComputedStyle = () => ({ lineHeight: '16px', fontSize: '16px' });
Object.defineProperty(globalThis, 'crypto', {
  value: { randomUUID: () => 'session-under-test' },
  configurable: true,
});
globalThis.fetch = (url, init) => {
  sent.push({ url, body: JSON.parse(init.body) });
  return Promise.resolve({ ok: true, json: () => Promise.resolve({}) });
};
class StubEventSource {
  constructor() { StubEventSource.latest = this; }
  close() {}
}
globalThis.EventSource = StubEventSource;

const module = new Function(`${script}\nreturn { apply, observe, takeControl, el, KEYS };`);
const page = module();

const deliver = message => page.apply(message);
const pane = { target: 'first', session: 'main', resource: 'w1:p1' };
const check = (what, condition) => {
  if (!condition) {
    console.error(`FAIL: ${what}`);
    process.exitCode = 1;
  } else {
    console.log(`ok: ${what}`);
  }
};

// Watching a pane: observing, and no keyboard offered.
page.observe(pane, 'first/main/w1:p1');
check('subscribes on observe', sent.some(one => one.body.type === 'pane.subscribe'));
check('no keyboard while observing', page.el('keyboard').hidden === true);
check('control is offered', page.el('control').hidden === false);

// The daemon confirms an observer lease: still no keyboard.
deliver({ type: 'pane.lease', pane, access: 'observe' });
check('observer lease keeps the keyboard away', page.el('keyboard').hidden === true);

// Typing is refused while observing, rather than silently dropped on the floor
// at the daemon.
sent.length = 0;
keyButtons[0].onclick();
check('an observer sends no input', sent.length === 0);

// Ask for control.
page.takeControl();
check('asks for control', sent.at(-1).body.type === 'pane.take_control');

// The daemon grants it.
deliver({ type: 'pane.lease', pane, access: 'control' });
check('the keyboard appears with control', page.el('keyboard').hidden === false);
check('control is no longer offered', page.el('control').hidden === true);

// A typed line arrives as bytes, with the Enter a shell is waiting for.
sent.length = 0;
page.el('line').value = 'ls -la';
page.el('line-form').onsubmit({ preventDefault() {} });
const input = sent.at(-1).body;
check('sends pane.input', input.type === 'pane.input');
check(
  'carries the line and a carriage return',
  Buffer.from(input.bytes, 'base64').toString('utf8') === 'ls -la\r',
);
check('clears the field', page.el('line').value === '');

// The key buttons.
for (const [index, key] of ['enter', 'tab', 'escape', 'interrupt', 'up', 'down'].entries()) {
  sent.length = 0;
  keyButtons[index].onclick();
  const bytes = Buffer.from(sent.at(-1).body.bytes, 'base64').toString('binary');
  check(`${key} sends ${JSON.stringify(page.KEYS[key])}`, bytes === page.KEYS[key]);
}

// Losing the lease takes the keyboard with it.
deliver({ type: 'pane.lease', pane, access: 'observe' });
check('a lost lease hides the keyboard', page.el('keyboard').hidden === true);

// Non-ASCII survives the trip.
deliver({ type: 'pane.lease', pane, access: 'control' });
sent.length = 0;
page.el('line').value = 'echo Sønderborg';
page.el('line-form').onsubmit({ preventDefault() {} });
check(
  'utf-8 survives',
  Buffer.from(sent.at(-1).body.bytes, 'base64').toString('utf8') === 'echo Sønderborg\r',
);

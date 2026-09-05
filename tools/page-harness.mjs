// Run app.html's real script against a stub DOM, and assert what it sends.
// The page is not executed in CI by decision (TESTING.md), so this is how a
// change to it gets run at all.
import { readFileSync } from 'node:fs';
import { createHash } from 'node:crypto';

const html = readFileSync(process.argv[2], 'utf8');
const script = html.slice(
  html.indexOf('<script>') + '<script>'.length,
  html.lastIndexOf('</script>'),
);

const sent = [];
const nodes = new Map();
let created = 0;
const node = id => {
  if (!nodes.has(id)) {
    const held = {
      id, hidden: false, textContent: '', value: '',
      className: '', style: {}, dataset: {}, children: [], disabled: false,
      append(...children) { this.children.push(...children); },
      remove() {},
      addEventListener(type, listener) { this[`on${type}`] = listener; },
      setAttribute(name, value) { this[name] = value; },
      focus() { this.focused = true; },
      select() { this.selected = true; },
      getBoundingClientRect: () => ({ width: 8, height: 16 }),
      clientWidth: 800,
      appendChild(child) { this.children.push(child); },
    };
    held.classList = {
      add(...names) {
        held.className = [...new Set(`${held.className} ${names.join(' ')}`.trim().split(/\s+/))]
          .filter(Boolean).join(' ');
      },
    };
    // A real element drops its children when its markup is replaced. The stub
    // has to as well: rendering a list twice would otherwise report the sum of
    // both renders, and every count assertion below would be measuring a bug
    // that does not exist in a browser.
    let markup = '';
    Object.defineProperty(held, 'innerHTML', {
      get: () => markup,
      set(value) {
        markup = value;
        if (value === '') held.children = [];
      },
    });
    held.querySelector = selector => node(`${id}-${selector}`);
    nodes.set(id, held);
  }
  return nodes.get(id);
};

const keyButtons = [
  'enter', 'escape', 'tab', 'up', 'down', 'left', 'right', 'backspace', 'interrupt',
].map(key => {
  const button = node(`key-${key}`);
  button.dataset = { key };
  return button;
});
const quickReplyButtons = ['yes', 'no', 'continue', 'retry'].map(reply => {
  const button = node(`reply-${reply}`);
  button.dataset = { reply };
  return button;
});
const codeBoxNodes = Array.from({ length: 8 }, (_, index) => node(`code-${index}`));

globalThis.document = {
  getElementById: node,
  createElement: () => node(`created-${created++}`),
  querySelector: selector => node(`sel-${selector}`),
  querySelectorAll: selector => {
    if (selector === '.keys button') return keyButtons;
    if (selector === '.quick-replies button') return quickReplyButtons;
    if (selector === '.code-box') return codeBoxNodes;
    return [];
  },
  body: node('body'),
  addEventListener() {},
};
const pathname = process.argv[3] || '/';
globalThis.window = {
  addEventListener() {},
  location: { href: `http://host${pathname}`, pathname, search: '' },
};
globalThis.location = globalThis.window.location;
globalThis.getComputedStyle = () => ({ lineHeight: '16px', fontSize: '16px' });
Object.defineProperty(globalThis, 'crypto', {
  value: {
    randomUUID: () => 'session-under-test',
    getRandomValues: values => { values[0] = 123456; return values; },
    subtle: {
      digest: async (_algorithm, bytes) => Uint8Array.from(
        createHash('sha256').update(Buffer.from(bytes)).digest(),
      ).buffer,
    },
  },
  configurable: true,
});
globalThis.fetch = (url, init) => {
  sent.push({ url, body: JSON.parse(init.body) });
  if (url.endsWith('/pair')) {
    return Promise.resolve({
      ok: false,
      status: 409,
      text: () => Promise.resolve('Choose a different device name and try this code again.'),
    });
  }
  return Promise.resolve({ ok: true, json: () => Promise.resolve({}) });
};
class StubEventSource {
  constructor() { StubEventSource.latest = this; }
  close() {}
}
globalThis.EventSource = StubEventSource;

const module = new Function(`${script}\nreturn { apply, observe, takeControl, uploadSelectedFiles, shellQuote, el, KEYS, QUICK_REPLIES, endpoint, renderPanes, codeBoxes, enteredCode };`);
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
const waitFor = async predicate => {
  for (let attempt = 0; attempt < 100; attempt++) {
    if (predicate()) return;
    await new Promise(resolve => setTimeout(resolve, 0));
  }
  throw new Error('timed out waiting for the page harness');
};

const expectedPrefix = pathname.startsWith('/r/') ? pathname.replace(/\/$/, '') : '';
check('requests stay on the served route', page.endpoint('/session') === `${expectedPrefix}/session`);
check('pairing uses eight separate code boxes', page.codeBoxes.length === 8);
let pastePrevented = false;
page.el('code').onpaste({
  clipboardData: { getData: () => 'abCD-2345' },
  preventDefault() { pastePrevented = true; },
});
check('a complete code pastes across all boxes', page.enteredCode() === 'ABCD2345');
check('pasting the code suppresses the one-field default', pastePrevented);
page.el('name').value = 'phone';
await page.el('pair').onclick();
check('a direct name collision explains how to retry', page.el('pair-error').textContent.includes('different'));
check('a direct name collision selects the used name', page.el('name').focused && page.el('name').selected);
check('a direct name collision keeps the same code', page.enteredCode() === 'ABCD2345');

// Navigation mirrors the identity hierarchy instead of flattening every pane.
deliver({
  type: 'state.full',
  state: {
    targets: [
      {
        key: { target: 'first', session: 'main' }, connection: 'live',
        snapshot: {
          workspaces: [{
            id: { target: 'first', session: 'main', resource: 'w1' },
            label: 'compiler',
          }],
          panes: [{
            id: pane,
            workspace: { target: 'first', session: 'main', resource: 'w1' },
            tab: { target: 'first', session: 'main', resource: 'w1:t1' },
            label: 'shell',
          }],
          agents: [{ name: 'builder', status: 'waiting', pane }],
        },
      },
      {
        key: { target: 'first', session: 'other' }, connection: 'connecting',
        snapshot: { workspaces: [], panes: [] },
      },
      {
        key: { target: 'second', session: 'main' }, connection: 'live',
        snapshot: { workspaces: [], panes: [] },
      },
    ],
  },
});
check('groups sessions under two targets', page.el('panes').children.length === 2);
const firstTarget = page.el('panes').children[0].children[0];
check('collapses target groups on a multi-target phone view', firstTarget.open === false);
check('keeps two sessions under the first target', firstTarget.children.length === 3);
check('shows one compact actionable attention item', page.el('attention').children.length === 1);
check('reports the compact attention count', page.el('attention-count').textContent === '1');
check('opens the first waiting attention batch', page.el('attention-panel').open === true);
check('does not waste a row on mark-all for one item', page.el('mark-actions').hidden === true);
check('attention items can open their terminal', typeof page.el('attention').children[0].onclick === 'function');
page.el('attention-panel').open = false;
deliver({
  type: 'attention.event',
  event: {
    id: 2, unread: true, agent: 'reviewer', kind: 'needs_input', workspace: 'review',
    pane: { target: 'first', session: 'main', resource: 'w1:p2' },
  },
});
check('preserves a manual attention collapse', page.el('attention-panel').open === false);
check('offers mark-all for a real attention batch', page.el('mark-actions').hidden === false);
deliver({ type: 'attention.history', events: [] });

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
check('reveals the keyboard during the control tap', page.el('keyboard').hidden === false);
check('focuses the line during the control tap', page.el('line').focused === true);
check('does not enable Send before the lease arrives', page.el('send').disabled === true);
check('does not enable terminal keys before the lease arrives', keyButtons.every(one => one.disabled));
check('does not enable quick replies before the lease arrives', quickReplyButtons.every(one => one.disabled));
sent.length = 0;
page.el('line').value = 'typed while waiting';
page.el('line-form').onsubmit({ preventDefault() {} });
check('sends nothing before control is granted', sent.length === 0);
check('keeps a line typed while waiting', page.el('line').value === 'typed while waiting');

// The daemon grants it.
deliver({ type: 'pane.lease', pane, access: 'control' });
check('the keyboard appears with control', page.el('keyboard').hidden === false);
check('control is no longer offered', page.el('control').hidden === true);
check('enables Send only with control', page.el('send').disabled === false);
check('enables terminal keys only with control', keyButtons.every(one => !one.disabled));
check('enables quick replies only with control', quickReplyButtons.every(one => !one.disabled));

// A browser file uses the daemon's verified upload protocol. Its MIME can be
// an Office type the clipboard-media table does not know; the name preserves
// the useful extension and the returned path is quoted before terminal input.
sent.length = 0;
const documentBytes = Buffer.from('pptx bytes under test');
const officeFile = {
  name: "Quarterly plan's (final).pptx",
  type: 'application/vnd.openxmlformats-officedocument.presentationml.presentation',
  size: documentBytes.length,
  arrayBuffer: async () => Uint8Array.from(documentBytes).buffer,
};
const uploading = page.uploadSelectedFiles([officeFile]);
check('disables another file choice while uploading', page.el('files').disabled === true);
await waitFor(() => sent.some(one => one.body.type === 'upload.begin'));
const beginning = sent.find(one => one.body.type === 'upload.begin').body;
check('offers the document to the pane under its real name', beginning.name === officeFile.name);
check('keeps the Office MIME type', beginning.mime === officeFile.type);
check('declares the exact document size', beginning.length === documentBytes.length);
deliver({
  type: 'upload.accepted', request: beginning.request, transfer: 'resume-token', staged: 0,
});
await waitFor(() => sent.some(one => one.body.type === 'upload.finish'));
const chunks = sent
  .filter(one => one.body.type === 'upload.chunk')
  .map(one => Buffer.from(one.body.bytes, 'base64'));
check('sends every document byte', Buffer.concat(chunks).equals(documentBytes));
const finishing = sent.find(one => one.body.type === 'upload.finish').body;
check(
  'attests to the bytes it sent',
  finishing.digest === createHash('sha256').update(documentBytes).digest('hex'),
);
const remoteDocument = "/tmp/super-herdr-clipboard.test/Quarterly plan's (final).pptx";
deliver({
  type: 'upload.complete', request: beginning.request, path: remoteDocument,
  bytes: documentBytes.length,
});
await uploading;
const pastedDocument = sent.filter(one => one.body.type === 'pane.input').at(-1).body;
check(
  'quotes the verified document path as one shell word',
  Buffer.from(pastedDocument.bytes, 'base64').toString('utf8') === page.shellQuote(remoteDocument),
);
check('re-enables file picking after completion', page.el('files').disabled === false);
check('reports verified browser upload completion', page.el('upload-note').textContent.includes('verified'));

const beforeOversized = sent.length;
await page.uploadSelectedFiles([{
  name: 'too-large.pdf', size: 32 * 1024 * 1024 + 1,
  arrayBuffer: async () => { throw new Error('an oversized file must not be read'); },
}]);
check('refuses an oversized phone file before reading it', sent.length === beforeOversized);
check('explains the phone file limit', page.el('upload-note').textContent.includes('32 MiB'));

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
for (const [index, key] of [
  'enter', 'escape', 'tab', 'up', 'down', 'left', 'right', 'backspace', 'interrupt',
].entries()) {
  sent.length = 0;
  keyButtons[index].onclick();
  const bytes = Buffer.from(sent.at(-1).body.bytes, 'base64').toString('binary');
  check(`${key} sends ${JSON.stringify(page.KEYS[key])}`, bytes === page.KEYS[key]);
}

// Quick replies are explicit one-tap submissions, not text inferred from the
// terminal screen. They include Enter and do not summon the software keyboard.
for (const [index, reply] of ['yes', 'no', 'continue', 'retry'].entries()) {
  sent.length = 0;
  page.el('line').focused = false;
  quickReplyButtons[index].onclick();
  const bytes = Buffer.from(sent.at(-1).body.bytes, 'base64').toString('utf8');
  check(`${reply} is submitted in one tap`, bytes === page.QUICK_REPLIES[reply]);
  check(`${reply} does not open the software keyboard`, page.el('line').focused === false);
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

// The agent inbox. Sections, ordering and membership are the daemon's answer;
// this page renders it and is checked for exactly that.
const card = (resource, overrides = {}) => ({
  agent: { target: 'first', session: 'main', resource },
  pane: { target: 'first', session: 'main', resource },
  title: resource,
  workspace: 'compiler',
  tab: 'build',
  pane_label: resource,
  provider: 'claude',
  activity: 'needs_input',
  status: 'blocked',
  section: 'needs_you',
  unread: false,
  stale: false,
  actionable: true,
  ...overrides,
});
const projection = overrides => ({
  type: 'agents.cards',
  projection: {
    version: 1, revision: 1, needs_you: [], working: [], recent: [], ...overrides,
  },
});
const sections = () => page.el('inbox-sections').children;
const cardsIn = index => sections()[index].children.slice(1);
const chipFor = (group, value) => page.el('inbox-filters').children
  .find(one => one.dataset.group === group && one.dataset.value === value);

deliver(projection({
  needs_you: [card('p2'), card('p1')],
  working: [card('p3', { status: 'working', activity: 'working', section: 'working' })],
}));
check('renders one block per non-empty section', sections().length === 2);
check(
  'keeps the order the daemon published',
  cardsIn(0).map(one => one.dataset.agent).join() === 'first/main/p2,first/main/p1',
);
check('titles a section with its count', sections()[0].children[0].textContent === 'Needs you (2)');
check('reports how many agents are waiting', page.el('inbox-count').textContent === '2 waiting');
check('offers the hierarchy as a way back', page.el('hierarchy') !== undefined);

// Filtering is a view of the same projection. It never asks the daemon for a
// different one, and it never changes what is being watched.
sent.length = 0;
chipFor('state', 'working').onclick();
check('a state filter narrows to one section', sections().length === 1);
check('a state filter keeps its section intact', cardsIn(0).length === 1);
check('filtering asks the daemon for nothing', sent.length === 0);
check(
  'a chip reports its own pressed state',
  chipFor('state', 'working')['aria-pressed'] === 'true'
    && chipFor('state', 'all')['aria-pressed'] === 'false',
);
chipFor('state', 'all').onclick();
check('clearing a filter restores every section', sections().length === 2);

// A dimension with one value offers no chip, because a control that cannot
// change anything is only something else to read on a phone.
check('offers no host chips for a single host', chipFor('target', 'all') === undefined);
deliver(projection({
  needs_you: [
    card('p1'),
    { ...card('p9'), agent: { target: 'second', session: 'main', resource: 'p9' },
      pane: { target: 'second', session: 'main', resource: 'p9' }, provider: 'codex' },
  ],
}));
check('offers host chips once there are two hosts', chipFor('target', 'second') !== undefined);
chipFor('target', 'second').onclick();
check('a host filter keeps only that host', cardsIn(0).length === 1);
check('a host filter keeps the qualified identity', cardsIn(0)[0].dataset.agent === 'second/main/p9');
chipFor('provider', 'codex') && chipFor('provider', 'codex').onclick();
check('an agent-kind filter composes with a host filter', cardsIn(0).length === 1);
chipFor('target', 'all').onclick();
chipFor('provider', 'all').onclick();

// A card that cannot resolve is shown and not offered.
deliver(projection({
  recent: [
    card('p8', { section: 'recent', actionable: false, pane: undefined, activity: 'gone', status: 'idle' }),
    card('p7', { section: 'recent', actionable: false, stale: true, status: 'blocked' }),
  ],
}));
check('history is not clickable', typeof cardsIn(0)[0].onclick !== 'function');
check('history says so to a screen reader', cardsIn(0)[0]['aria-disabled'] === 'true');
check('history is disabled', cardsIn(0)[0].disabled === true);
check('a stale card explains the host, not the agent', cardsIn(0)[1].children[1].children[1].textContent === 'host not connected');

// Opening a card routes to its own qualified pane and settles its attention.
deliver(projection({ needs_you: [card('p1', { unread: true })] }));
sent.length = 0;
cardsIn(0)[0].onclick();
check(
  'opening a card marks its attention seen',
  sent.some(one => one.body.type === 'attention.mark_seen' && one.body.pane.resource === 'p1'),
);
check(
  'opening a card subscribes to its exact qualified pane',
  sent.some(one => one.body.type === 'pane.subscribe'
    && one.body.pane.target === 'first' && one.body.pane.resource === 'p1'),
);
const watched = page.el('viewing').textContent;

// Background churn repaints the inbox and must not move what a person is
// reading. The daemon re-resolves before it routes; the page must not
// re-route on its own.
sent.length = 0;
page.el('line').focused = false;
deliver(projection({
  revision: 2,
  needs_you: [card('p4'), card('p1', { unread: true })],
  working: [card('p5', { status: 'working', section: 'working' })],
}));
check('a repaint keeps watching the same pane', page.el('viewing').textContent === watched);
check('a repaint subscribes to nothing', sent.every(one => one.body.type !== 'pane.subscribe'));
check('a repaint takes no focus', page.el('line').focused === false);

// A reconnect asks for the inbox again, because the daemon remembers nothing
// about what this page held.
sent.length = 0;
deliver({ type: 'server.hello', protocol: 3, server_version: '0.7.20', features: ['terminal', 'agent_cards'] });
check('a reconnect resubscribes to the inbox', sent.some(one => one.body.type === 'agents.subscribe'));

// An older daemon has no inbox to send. Say so instead of waiting forever.
sent.length = 0;
deliver({ type: 'server.hello', protocol: 3, server_version: '0.7.20', features: ['terminal'] });
check('an older daemon asks for no inbox', sent.every(one => one.body.type !== 'agents.subscribe'));
check('an older daemon hides the inbox', page.el('inbox').hidden === true);
check('an older daemon opens the hierarchy instead', page.el('hierarchy').open === true);
deliver({ type: 'server.hello', protocol: 3, server_version: '0.7.20', features: ['terminal', 'agent_cards'] });
page.el('inbox').hidden = false;

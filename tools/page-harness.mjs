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
      removeAttribute(name) { delete this[name]; },
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
const codeBoxNodes = Array.from({ length: 8 }, (_, index) => node(`code-${index}`));

globalThis.document = {
  getElementById: node,
  createElement: () => node(`created-${created++}`),
  querySelector: selector => node(`sel-${selector}`),
  querySelectorAll: selector => {
    if (selector === '.keys button') return keyButtons;
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
// A notification the page raised, and the permission it was raised under.
const alerts = [];
class StubNotification {
  constructor(title, options) {
    this.title = title;
    this.options = options;
    alerts.push(this);
  }
}
StubNotification.permission = 'default';
StubNotification.requestPermission = () => {
  StubNotification.permission = StubNotification.granting ? 'granted' : 'denied';
  return Promise.resolve(StubNotification.permission);
};
globalThis.Notification = StubNotification;

class StubEventSource {
  constructor() { StubEventSource.latest = this; }
  close() {}
}
globalThis.EventSource = StubEventSource;
// Object URLs are a browser thing; the page revokes what it creates, so the
// stub counts both to catch a blob left behind.
let objectUrls = 0;
globalThis.URL = class extends URL {
  static createObjectURL() {
    objectUrls++;
    return `blob:stub/${objectUrls}`;
  }

  static revokeObjectURL() {
    objectUrls--;
  }
};

const module = new Function(`${script}\nreturn { apply, observe, takeControl, uploadSelectedFiles, shellQuote, el, KEYS, endpoint, renderPanes, codeBoxes, enteredCode, stopWatching };`);
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
const replies = () => page.el('quick-replies').children;
const configureReplies = quick_replies => deliver({
  type: 'server.hello',
  protocol: 3,
  server_version: '0.7.20',
  features: ['terminal', 'agent_cards'],
  quick_replies,
});

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

// The daemon said which replies it offers during the handshake.
configureReplies([
  { label: 'Yes', send: 'y', submit: true, confirm: false },
  { label: 'No', send: 'n', submit: true, confirm: false },
  { label: 'Paste path', send: '/srv/build', submit: false, confirm: false },
  { label: 'Wipe', send: 'reset --hard', submit: true, confirm: true },
]);

// Ask for control.
page.takeControl();
check('asks for control', sent.at(-1).body.type === 'pane.take_control');
check('reveals the keyboard during the control tap', page.el('keyboard').hidden === false);
check('focuses the line during the control tap', page.el('line').focused === true);
check('does not enable Send before the lease arrives', page.el('send').disabled === true);
check('does not enable terminal keys before the lease arrives', keyButtons.every(one => one.disabled));
check(
  'does not enable quick replies before the lease arrives',
  replies().length > 0 && replies().every(one => one.disabled),
);
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
check(
  'enables quick replies only with control',
  replies().length > 0 && replies().every(one => !one.disabled),
);

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

// Quick replies are what the daemon was configured to offer, rendered exactly
// as sent. Nothing here is inferred from the terminal screen.

check('renders one button per configured reply', replies().length === 4);
check('a reply is labelled as configured', replies()[0].textContent === 'Yes');
check(
  'a reply says what it sends',
  replies()[0]['aria-label'] === 'Send y and Enter'
    && replies()[2]['aria-label'] === 'Send /srv/build',
);

for (const [index, expected] of [['Yes', 'y\r'], ['No', 'n\r'], ['Paste path', '/srv/build']].entries()) {
  sent.length = 0;
  page.el('line').focused = false;
  replies()[index].onclick();
  const bytes = Buffer.from(sent.at(-1).body.bytes, 'base64').toString('utf8');
  check(`${expected[0]} is submitted in one tap`, bytes === expected[1]);
  check(`${expected[0]} does not open the software keyboard`, page.el('line').focused === false);
}

// A reply that declared it needs confirming takes two taps, on itself.
sent.length = 0;
replies()[3].onclick();
check('a confirming reply sends nothing on the first tap', sent.length === 0);
check('a confirming reply says it is armed', replies()[3]['aria-pressed'] === 'true');
check('a confirming reply asks for the second tap', replies()[3].textContent === 'Tap again');
replies()[3].onclick();
check(
  'a confirming reply sends on the second tap',
  Buffer.from(sent.at(-1).body.bytes, 'base64').toString('utf8') === 'reset --hard\r',
);
check('a sent reply disarms', replies()[3].textContent === 'Wipe');

// Losing the lease ends the moment an armed reply belonged to.
replies()[3].onclick();
deliver({ type: 'pane.lease', pane, access: 'observe' });
check('a lost lease disarms a waiting reply', replies()[3].textContent === 'Wipe');
sent.length = 0;
replies()[0].onclick();
check('an observer sends no reply', sent.length === 0);
deliver({ type: 'pane.lease', pane, access: 'control' });

// A daemon that offers none draws none. The page does not invent a fallback,
// because a button nobody configured would be a guess about what to type.
configureReplies([]);
check('no configured replies draws no buttons', replies().length === 0);
check('an empty strip is hidden', page.el('quick-replies').hidden === true);
configureReplies([{ label: 'Yes', send: 'y', submit: true, confirm: false }]);
deliver({ type: 'pane.lease', pane, access: 'control' });

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
// A card is a container: the primary action first, then the marks a person can
// put on it. Reaching through it here keeps the assertions about behaviour
// rather than about which element happens to carry the handler.
const openOf = one => one.children[0];
const marksOf = one => (one.children[1] ? one.children[1].children : []);
const markChip = (one, kind) => marksOf(one).find(chip => chip.dataset.mark === kind);
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
check('history is not clickable', typeof openOf(cardsIn(0)[0]).onclick !== 'function');
check('history says so to a screen reader', openOf(cardsIn(0)[0])['aria-disabled'] === 'true');
check('history is disabled', openOf(cardsIn(0)[0]).disabled === true);
check(
  'a stale card explains the host, not the agent',
  openOf(cardsIn(0)[1]).children[1].children[1].textContent === 'host not connected',
);
check('a gone agent offers nothing to mark', marksOf(cardsIn(0)[0]).length === 0);
check('a stale card still offers its marks', marksOf(cardsIn(0)[1]).length === 3);

// Opening a card routes to its own qualified pane and settles its attention.
deliver(projection({ needs_you: [card('p1', { unread: true })] }));
sent.length = 0;
openOf(cardsIn(0)[0]).onclick();
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

// Pins, mutes and snoozes are requests to the daemon, and never anything that
// reaches the host.
deliver(projection({ needs_you: [card('p1'), card('p2')] }));
sent.length = 0;
markChip(cardsIn(0)[1], 'pin').onclick();
const pinRequest = sent.find(one => one.body.type === 'agents.mark');
check('pinning names the qualified agent', pinRequest.body.agent.resource === 'p2'
  && pinRequest.body.agent.target === 'first' && pinRequest.body.agent.session === 'main');
check('pinning asks for a pin', pinRequest.body.mark.kind === 'pin' && pinRequest.body.mark.pinned === true);
check('a mark is the only thing sent', sent.length === 1);

sent.length = 0;
markChip(cardsIn(0)[0], 'snooze').onclick();
const snoozeRequest = sent.find(one => one.body.type === 'agents.mark');
check('a snooze asks for a duration, never a moment',
  snoozeRequest.body.mark.kind === 'snooze' && snoozeRequest.body.mark.minutes === 60);

// The daemon answers by republishing the inbox; the page renders that and
// never assumes its own request landed.
deliver(projection({
  needs_you: [
    card('p2', { marks: { pinned: true, muted: false } }),
    card('p1'),
  ],
}));
check('a pinned card reports itself pressed', markChip(cardsIn(0)[0], 'pin')['aria-pressed'] === 'true');
check('a pinned card names the state it is in', markChip(cardsIn(0)[0], 'pin').textContent === 'Pinned');
check('an unpinned card stays unpressed', markChip(cardsIn(0)[1], 'pin')['aria-pressed'] === 'false');
check('a pinned agent offers a pinned filter', chipFor('state', 'pinned') !== undefined);
chipFor('state', 'pinned').onclick();
check('the pinned filter keeps only pinned cards', cardsIn(0).length === 1);
check('the pinned filter keeps the qualified identity', cardsIn(0)[0].dataset.agent === 'first/main/p2');
sent.length = 0;
markChip(cardsIn(0)[0], 'pin').onclick();
check('unpinning asks to clear the pin',
  sent.find(one => one.body.type === 'agents.mark').body.mark.pinned === false);
deliver(projection({ needs_you: [card('p2'), card('p1')] }));
check('a filter that can no longer match anything steps aside', cardsIn(0).length === 2);

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

// Alerts are two permissions, granted at different moments: the browser's, and
// this page's subscription to the daemon. Rendering the inbox is neither.
StubNotification.granting = false;
alerts.length = 0;
sent.length = 0;
await page.el('alerts').onclick();
check('a refused browser permission subscribes to nothing',
  sent.every(one => one.body.type !== 'notifications.subscribe'));
check('a refused browser permission says so', page.el('alert-note').textContent.includes('not allowing'));
check('a refused permission leaves the toggle off', page.el('alerts')['aria-pressed'] === 'false');

StubNotification.granting = true;
sent.length = 0;
await page.el('alerts').onclick();
check('a granted permission subscribes',
  sent.some(one => one.body.type === 'notifications.subscribe'));
check('a granted permission shows the toggle on', page.el('alerts')['aria-pressed'] === 'true');

// An alert carries bounded metadata and replaces its own agent's last one.
deliver(projection({ needs_you: [card('p1', { unread: true })] }));
alerts.length = 0;
deliver({
  type: 'notification',
  agent: { target: 'first', session: 'main', resource: 'p1' },
  title: 'Agent needs attention',
  body: 'reviewer · compiler',
});
check('an alert is raised', alerts.length === 1);
check('an alert carries the daemon title', alerts[0].title === 'Agent needs attention');
check('an alert is tagged by its agent', alerts[0].options.tag === 'first/main/p1');

// Tapping it opens the agent it names, against the inbox as it is now.
sent.length = 0;
alerts[0].onclick();
check(
  'tapping an alert opens the agent it names',
  sent.some(one => one.body.type === 'pane.subscribe' && one.body.pane.resource === 'p1'),
);
check('an opened alert clears the note', page.el('alert-note').textContent === '');

// The race the exit condition names: the agent went away between the alert
// being sent and the tap landing.
deliver(projection({ needs_you: [] }));
sent.length = 0;
alerts[0].onclick();
check('a tap on a departed agent opens nothing',
  sent.every(one => one.body.type !== 'pane.subscribe'));
check('a tap on a departed agent says so',
  page.el('alert-note').textContent === 'That agent is no longer running.');

// An agent still listed but not reachable — its host dropped — is a different
// sentence, because the agent has not ended.
deliver(projection({
  needs_you: [card('p1', { actionable: false, stale: true })],
}));
sent.length = 0;
alerts[0].onclick();
check('a tap on an unreachable agent opens nothing',
  sent.every(one => one.body.type !== 'pane.subscribe'));
check('a tap on an unreachable agent distinguishes the host',
  page.el('alert-note').textContent === 'That agent is no longer reachable.');

// Turning them off ends the daemon's side, and a stray alert is ignored.
sent.length = 0;
await page.el('alerts').onclick();
check('turning alerts off unsubscribes',
  sent.some(one => one.body.type === 'notifications.unsubscribe'));
alerts.length = 0;
deliver({
  type: 'notification',
  agent: { target: 'first', session: 'main', resource: 'p1' },
  title: 'Agent needs attention',
  body: 'reviewer · compiler',
});
check('an alert after turning them off raises nothing', alerts.length === 0);
StubNotification.granting = true;
await page.el('alerts').onclick();

// Fetching a file off a target. Two steps: the daemon says what the file is,
// and nothing crosses the link until a person accepts it.
const digestOf = async bytes => Buffer.from(
  await crypto.subtle.digest('SHA-256', bytes),
).toString('hex');
// The request number is remembered rather than re-read, because the tests
// clear `sent` between steps the way a real client forgets nothing.
let fetchRequest = 0;
const askFor = path => {
  page.el('fetch-path').value = path;
  page.el('fetch-form').onsubmit({ preventDefault() {} });
  fetchRequest = sent.find(one => one.body.type === 'download.begin').body.request;
};
const offer = (name, length, digest) => deliver({
  type: 'download.offer', request: fetchRequest, name, length, digest,
});
const arrive = bytes => deliver({
  type: 'download.chunk', request: fetchRequest, bytes: bytes.toString('base64'),
});
const finish = () => deliver({ type: 'download.finished', request: fetchRequest });

page.observe(pane, 'first/main/w1:p1');
sent.length = 0;
askFor('/srv/build/report.txt');
const begun = sent.find(one => one.body.type === 'download.begin');
check('asking for a file names the pane and the typed path',
  begun.body.pane.resource === 'w1:p1' && begun.body.path === '/srv/build/report.txt');
check('asking for a file pulls nothing yet',
  sent.every(one => one.body.type !== 'download.pull'));

const body = Buffer.from('line one\nline two\n');
const digest = await digestOf(body);
offer('report.txt', body.length, digest);
check('the offer is shown before any bytes move', page.el('fetch-offer').hidden === false);
check('the offer names the file', page.el('fetch-name').textContent === 'report.txt');
check('the offer gives the size', page.el('fetch-size').textContent === '18 B');
check('the offer names the source', page.el('fetch-where').textContent === 'first/main · /srv/build/report.txt');
check('the offer shows the host digest', page.el('fetch-digest').textContent.startsWith('sha256 '));
check('an unaccepted offer has pulled nothing',
  sent.every(one => one.body.type !== 'download.pull'));

sent.length = 0;
page.el('fetch-accept').onclick();
check('accepting pulls a bounded window',
  sent.some(one => one.body.type === 'download.pull' && one.body.chunks === 4));
arrive(body);
finish();
// The digest is verified asynchronously, so wait for the verdict rather than
// for a flag the stub starts out holding.
await waitFor(() => page.el('fetch-note').textContent.includes('verified'));
check('a verified file is offered for saving', page.el('fetch-save')['download'] === 'report.txt');
check('a verified file says so', page.el('fetch-note').textContent.includes('verified'));
check('a text file is previewed as text',
  page.el('fetch-text').hidden === false
    && page.el('fetch-text').textContent === 'line one\nline two\n');
check('a text preview is not an image', page.el('fetch-image').hidden === true);
page.el('fetch-discard').onclick();
check('discarding releases the object URL', objectUrls === 0);

// An image is shown as an image, decided from the name because a download
// carries no declared type. A name matching nothing is saved and not previewed.
sent.length = 0;
askFor('/srv/build/shot.png');
const image = Buffer.from('89504e470d0a1a0a', 'hex');
offer('shot.png', image.length, await digestOf(image));
page.el('fetch-accept').onclick();
arrive(image);
finish();
await waitFor(() => page.el('fetch-note').textContent.includes('verified'));
check('an image is previewed as an image', page.el('fetch-image').hidden === false);
check('an image preview is not text', page.el('fetch-text').hidden === true);
page.el('fetch-discard').onclick();

sent.length = 0;
askFor('/srv/build/archive.tar.zst');
offer('archive.tar.zst', body.length, digest);
page.el('fetch-accept').onclick();
arrive(body);
finish();
await waitFor(() => page.el('fetch-note').textContent.includes('verified'));
check('an unrecognised type is saved and not previewed',
  page.el('fetch-text').hidden === true && page.el('fetch-image').hidden === true);
check('an unrecognised type is still offered for saving',
  page.el('fetch-save')['download'] === 'archive.tar.zst');
page.el('fetch-discard').onclick();

// A file that does not match what its host attested is discarded, not offered
// with a warning.
sent.length = 0;
askFor('/srv/build/report.txt');
offer('report.txt', body.length, 'f'.repeat(64));
page.el('fetch-accept').onclick();
arrive(body);
finish();
await waitFor(() => page.el('fetch-note').textContent.includes('digest'));
check('a file failing its digest is discarded', page.el('fetch-result').hidden === true);
check('a file failing its digest says why',
  page.el('fetch-note').textContent.includes('did not match'));
check('a discarded file leaves no object URL', objectUrls === 0);

// A transfer that stops short is distinguishable from one still arriving,
// which is what the declared length is for.
sent.length = 0;
askFor('/srv/build/report.txt');
offer('report.txt', body.length, digest);
page.el('fetch-accept').onclick();
arrive(body.subarray(0, 4));
finish();
await waitFor(() => page.el('fetch-note').textContent.includes('stopped'));
check('a short transfer is reported, not saved', page.el('fetch-result').hidden === true);

// Too large to hold is refused before a byte moves.
sent.length = 0;
askFor('/srv/build/core.dump');
offer('core.dump', 64 * 1024 * 1024, digest);
check('an oversized file cannot be accepted', page.el('fetch-accept').disabled === true);
check('an oversized file says why', page.el('fetch-note').textContent.includes('Too large'));
check('an oversized file pulls nothing',
  sent.every(one => one.body.type !== 'download.pull'));
page.el('fetch-refuse').onclick();
check('refusing an offer cancels it',
  sent.some(one => one.body.type === 'download.cancel'));

// A fetch belongs to the pane it was asked for.
sent.length = 0;
askFor('/srv/build/report.txt');
offer('report.txt', body.length, digest);
sent.length = 0;
page.observe({ target: 'first', session: 'main', resource: 'w1:p2' }, 'other');
check('watching another pane cancels a fetch',
  sent.some(one => one.body.type === 'download.cancel'));
check('an abandoned fetch clears its offer', page.el('fetch-offer').hidden === true);

// A reconnect leaves the daemon holding no such request, so this end forgets
// it rather than cancelling a number that now means something else.
page.observe(pane, 'first/main/w1:p1');
sent.length = 0;
askFor('/srv/build/report.txt');
offer('report.txt', body.length, digest);
sent.length = 0;
deliver({
  type: 'server.hello', protocol: 3, server_version: '0.7.20',
  features: ['terminal', 'agent_cards'], quick_replies: [],
});
check('a reconnect does not cancel a request the daemon has forgotten',
  sent.every(one => one.body.type !== 'download.cancel'));
check('a reconnect reports the ended transfer',
  page.el('fetch-note').textContent.includes('connection'));

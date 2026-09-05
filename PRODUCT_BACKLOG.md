# Super-Herdr product backlog

This backlog turns the recurring needs seen in the Herdr community and the
useful interaction patterns in CCGram into work that fits Super-Herdr's trust
model. It supplements `ROADMAP.md`; the operational and real-device
qualification gates there still come first.

## Product decision

Super-Herdr should be the cross-machine inbox and control plane for Herdr, not
another generic mobile terminal and not another agent runtime.

The default browser experience should answer three questions immediately:

1. Which agent needs me?
2. What safe, explicit response can I send?
3. Which machine and Herdr session will receive it?

The existing target/session/workspace/tab/pane hierarchy remains the source of
navigation truth. The agent inbox is a task-oriented projection of that truth,
optimized for a small screen.

## What "topic per agent" means

CCGram uses a Telegram topic as the durable place where a person returns to one
agent. Super-Herdr should adopt the mental model without adopting Telegram,
transcript storage, or CCGram's lifecycle behavior.

In Super-Herdr, a topic is a stable card in the browser and TUI:

```text
Federation hierarchy                         Agent inbox

target                                      Needs you
└── session                                 ├── agent card
    └── workspace                           └── agent card
        └── tab
            └── pane ── agent session  ->  Working
                                             ├── agent card
                                             └── agent card

                                            Recent
                                             └── non-actionable history card
```

Each live card resolves to one qualified route:

```text
target + Herdr session + agent session + current pane
```

The target and Herdr session are part of the identity. Workspace, tab, pane,
provider, and user-facing labels are display and routing metadata; names are
never used as identity. The daemon must resolve the current pane again before
every action and fail closed if the agent disappeared, became ambiguous, or
moved outside the expected qualified session.

The model has these UX rules:

- Opening the browser lands on the agent inbox, with **Needs you** first.
- A card stays in the same section and relative position during unrelated
  target churn whenever possible.
- A connected or disconnected server never changes the pane receiving the
  user's next byte.
- Opening a card observes the pane. Sending input still requires an explicit
  control lease.
- A card can be pinned, muted, or snoozed without changing the Herdr object.
- A disappeared agent can remain in recent history, but its historical card is
  never an input target.
- Cards show bounded metadata only. Terminal contents and transcripts are not
  persisted, indexed, summarized, or logged.
- This remains a single-operator model. A paired personal device is not a team
  member or delegated user.

This makes the product easier to understand: the hierarchy answers "where is
it?" while the inbox answers "what needs me now?"

## Delivery order

### 0. Make the current baseline installable and qualified

- [ ] Complete the deployed-bridge and real-phone qualification in
      `ROADMAP.md` before treating the hosted path as dependable.
- [ ] Run the required release checks on the merged plugin-action build.
- [ ] Publish the next tagged release so Homebrew, Debian, and archive users
      receive the plugin actions already merged after `v0.7.20`.
- [ ] Verify the Homebrew formula and every published binary from the release.
- [ ] Record the exact released commit and phone/browser matrix in
      `TESTING.md`.

Exit condition: a new user can install the latest release, pair a real phone on
an unrelated network, open an existing pane, invoke a plugin action, and retain
the exact selected route while targets connect or disconnect.

### 1. Agent inbox

#### Daemon and protocol

- [x] Define a versioned `AgentCard` projection owned by the daemon.
- [x] Key each live card by target, Herdr session, and agent-session identity.
- [x] Include bounded display metadata: target label, workspace, tab, pane,
      provider, agent state, attention state, and last state-change time.
- [x] Publish deterministic sections: needs attention, active, and recent.
- [x] Preserve card ordering across unrelated federation refreshes.
- [ ] Resolve the live pane again before every card action and fail closed on a
      missing or ambiguous route. The resolver and its refusals exist; this
      closes when a card action calls it.
- [x] Add persistence for pins, mutes, and snoozes using qualified identities.
- [x] Keep historical cards non-actionable after their agent disappears.
- [x] Add protocol and daemon tests for duplicate server-local IDs, target
      churn, agent moves, and stale cards.

#### Browser

- [x] Make the inbox the default paired-device screen.
- [x] Give each card one primary action: open the live pane.
- [x] Add compact filters for needs-attention, active, pinned, target, and
      provider.
- [x] Add a clear path back to the full infrastructure hierarchy.
- [ ] Add an optional pinned-agent grid for monitoring several panes.
- [x] Keep cards readable and controls reachable at narrow phone widths.
- [x] Preserve the open card and its exact qualified pane during background
      target churn.
- [x] Extend `tools/page-harness.mjs` for ordering, filtering, reconnection,
      accessibility, and non-focus-stealing behavior.

#### TUI

- [ ] Reuse the daemon projection in the existing agent navigator. The TUI
      subscribes and mirrors each agent's marks; the navigator still derives
      its own list from federation state.
- [x] Show pin, mute, and snooze actions in qualified context menus and the
      searchable action palette.
- [x] Keep the full hierarchy available; do not replace expert navigation with
      the inbox.

Exit condition: a person with agents across several machines can identify and
open the next agent needing attention without browsing a long hierarchy, and
no connection event can redirect input.

### 2. Structured response actions

**The dependency below resolved against structured choices.** Herdr's API
schema (checked at protocol 19, `herdr api schema --json`) reports an agent's
status and whether it is ready for input, and describes nowhere what a blocked
agent is asking: there is no choice, option, confirm, or approve concept in its
request or event schemas. Semantic response buttons therefore stay blocked, and
this step ships user-configured quick replies instead.

- [x] Do not infer buttons by scraping or parsing terminal output.
- [x] Keep Enter, Escape, Tab, Ctrl-C, history arrows, and customizable quick
      replies as explicit terminal controls.
- [x] Require the pane control lease for any response that becomes terminal
      input.
- [x] Confirm actions whose structured metadata declares a destructive or
      irreversible effect. With no such metadata to read, the person who wrote
      the reply declares it with `confirm`.
- [x] Test stale choices, lease loss, and retries.
- [ ] Blocked on Herdr: a protocol representation for bounded response choices
      with a stable action ID, label, qualified agent route, expiry, and action
      kind.
- [ ] Blocked on Herdr: accept structured choices only from documented Herdr or
      plugin metadata.
- [ ] Blocked on Herdr: render common choices as compact buttons — yes, no,
      approve, deny, and numbered selections.
- [ ] Blocked on Herdr: expire buttons when the prompt changes. Agent, session
      and lease changes already disarm a waiting reply.

Exit condition: a phone can answer with one configured tap, a reply that
declares itself irreversible takes two, and nothing reaches a pane without the
control lease.

### 3. Paired-device notifications

- [x] Add opt-in delivery as another sink under the existing attention filters,
      coalescing, and rate limits.
- [x] Add per-agent modes: default, needs-attention only, muted, and temporary
      snooze.
- [x] Put only bounded metadata in notification payloads.
- [x] Make a notification open the exact qualified live card or report that it
      is no longer available.
- [x] Never embed terminal output, clipboard data, filenames, pairing material,
      or secrets in a notification.
- [x] Document browser/platform requirements and a deterministic test path.
- [x] Test duplicate delivery, unsubscribed devices, expired routes, target
      disconnects, and notification-click races.
- [ ] Reach a device whose browser is closed. This needs Web Push: a service
      worker, VAPID signing, and a third-party push service as a new outbound
      trust boundary whose endpoint the browser supplies. The design that fits
      here sends no payload and lets the woken service worker ask the daemon,
      so the push service learns that something happened and never what. It is
      a dependency and trust decision, not a detail of the sink, and wants its
      own branch.

Exit condition: a paired device receives one useful attention alert under the
same filters as the desktop and returns to the correct agent without exposing
payloads. Receiving one with the browser closed waits on the item above.

### 4. Remote files and artifacts

- [x] Finish browser upload and target-to-device download surfaces on the
      existing transfer protocol.
- [x] Finish TUI target-to-client save and target-to-target transfer surfaces.
- [x] Add an explicit, bounded remote-path picker scoped to one qualified
      target and session.
- [x] Support bounded filename, substring, and opt-in glob search without
      traversing beyond configured roots.
- [x] Show metadata before transfer: name, type, size, source target, and
      digest when available.
- [x] Add safe client-side previews for text and images after the normal
      bounded transfer and digest verification. A PDF is saved rather than
      rendered: previewing one hands a file off somebody's host to the
      browser's scripted viewer inside this page, which wants its own decision.
- [ ] Route Git diff, source browsing, Office rendering, and richer review
      experiences through plugin actions rather than duplicating an IDE.
- [x] Add cancellation and cleanup tests for every transfer direction.
- [x] Never discover "files the agent read" by parsing transcripts or terminal
      contents; require an explicit path or structured plugin result.

Exit condition: a phone user can find a permitted remote file, verify its
source and size, download or preview it, and send an uploaded file to exactly
one controlled pane.

### 5. Unified diagnostics

- [ ] Add a read-only `super-herdr doctor` command.
- [ ] Check configuration permissions, Herdr executable/protocol compatibility,
      SSH aliases, each target independently, the daemon socket, bridge route,
      pairing prerequisites, clipboard tools, notification capability, and
      transfer dependencies.
- [ ] Bound and time out every network check.
- [ ] Redact host details, credentials, authorization headers, private routes,
      terminal contents, and pairing material from output.
- [ ] Report corrective commands instead of changing the system automatically.
- [ ] If a later `--fix` mode is added, require a separate confirmation for
      every mutation and never stop or restart a Herdr session.
- [ ] Add machine-readable output for support bundles containing metadata only.

Exit condition: a user can identify which layer is broken without exposing
private material or affecting a healthy target.

### 6. Cross-host plugin inventory and explicit sync

- [ ] Read installed plugin inventory through documented Herdr CLI/socket
      interfaces for each qualified target and session.
- [ ] Show missing plugins and version drift without treating same-named
      server-local plugin IDs as globally identical.
- [ ] Add marketplace search and plugin-detail links.
- [ ] Export a desired plugin set with pinned references and a lockfile.
- [ ] Produce an installation/update plan before making changes.
- [ ] Apply only to explicitly selected targets after confirmation.
- [ ] Isolate errors per target and never roll back by stopping or restarting a
      Herdr session.
- [ ] Do not import Herdr internals or duplicate its plugin installer.

Exit condition: the operator can see and deliberately reconcile plugin drift
across machines while one failing target leaves every other target usable.

### 7. Aliases, favourites, and saved views

- [ ] Add Super-Herdr-local display aliases for qualified agents, workspaces,
      panes, sessions, and targets.
- [ ] Never use an alias as routing identity.
- [ ] Add pinned workspaces and favourite qualified destinations.
- [ ] Add optional numbered jump slots without renaming Herdr resources.
- [ ] Save inbox filters and the preferred landing view per client.
- [ ] Surface an explicit Herdr rename action separately when the documented
      interface supports it; never rename automatically.
- [ ] Handle deleted, moved, and duplicate-looking resources without silently
      retargeting an alias.

Exit condition: frequently used projects and agents are one action away while
all operations remain bound to qualified live identities.

### 8. Multi-host onboarding and network choices

- [ ] Offer an OpenSSH-config alias importer with a preview and explicit target
      selection.
- [ ] Support adding and probing several targets in one wizard with independent
      results and timeouts.
- [ ] Add local tags and groups such as work, home, lab, and pods.
- [ ] Explain hosted bridge, direct LAN, Tailscale, NetBird/WireGuard-style
      private routes, and an operator-managed proxy as peer choices.
- [ ] Make the default hosted route clear: it needs no Tailscale but remains a
      trusted relay because TLS terminates there.
- [ ] Investigate a native Windows control-plane build that can supervise SSH
      targets without claiming native Herdr support or depending on Herdr
      internals.
- [ ] Keep WSL2 as the documented Windows fallback until the native matrix is
      complete.

Exit condition: a new user with several existing SSH hosts can add them without
hand-writing TOML and can understand which network trust boundary they chose.

### 9. Remote development preview, after the core path is qualified

- [ ] Write a threat model before implementation, covering SSRF, private-network
      traversal, redirects, cookies, authentication headers, response size,
      content type, and malicious target services.
- [ ] Require an explicit, expiring grant for one target-local origin.
- [ ] Restrict the first version to configured loopback origins on the selected
      target.
- [ ] Bound connections, redirects, headers, bodies, frame rates, and lifetime.
- [ ] Keep preview traffic isolated from terminal control and other targets.
- [ ] Do not log URLs containing secrets, headers, cookies, or response bodies.
- [ ] Revoke the preview immediately when the device, route, or grant is
      revoked.

Exit condition: a paired device can deliberately open one approved development
origin without becoming a general-purpose proxy into the target network.

## Explicit non-goals

The following ideas are useful, but do not belong in Super-Herdr's core:

- Telegram or another hosted chat service as the control transport.
- Persisted transcript delivery, transcript search, or terminal-content
  summaries.
- Automatic session start, recovery, restart, replacement, or cleanup.
- Provider-specific agent runtimes or orchestration.
- Native DAG, cost-tracking, IDE explorer, code-review, command-policy, theme,
  or automatic-title implementations. These should remain Herdr plugins whose
  documented actions and panes Super-Herdr exposes consistently.
- Heuristic interpretation of terminal output as approvals, choices, file
  paths, commands, or security policy.
- Team sharing, delegated authority, or audit products before the
  single-operator desktop and paired-device paths are fully qualified.
- Server-side voice recording or transcription by default. Device dictation is
  sufficient for the first iteration; any later transcription service must be
  explicit and disclose its data boundary.

## Suggested branch sequence

Keep each branch small enough to review and qualify independently:

1. `feat/agent-card-protocol`
2. `feat/browser-agent-inbox`
3. `feat/agent-pins-and-filters`
4. `feat/structured-response-actions`
5. `feat/paired-device-push`
6. `feat/browser-file-downloads`
7. `feat/remote-file-picker`
8. `feat/super-herdr-doctor`
9. `feat/plugin-fleet-inventory`
10. `feat/local-aliases-and-favourites`
11. `feat/ssh-target-import`

Remote development preview and native Windows support require separate design
reviews before an implementation branch is opened.

## Research signals

- Herdr users repeatedly describe mobile SSH/TUI use as painful and want a
  simple way to check and steer existing agents.
- Agent-first mobile clients prioritize blocked agents, direct prompt choices,
  quick actions, notifications, files, and stable per-agent destinations.
- Herdr's growing plugin ecosystem creates demand for discovery, consistent
  installation across machines, and access to plugin surfaces remotely.
- Requests for remote file inspection, automatic labels, favourite projects,
  command palettes, and cross-machine setup all point to reducing navigation
  and recall rather than adding another orchestration engine.

Primary examples:

- <https://github.com/alexei-led/ccgram>
- <https://www.reddit.com/r/herdr/comments/1vxsh7w/>
- <https://www.reddit.com/r/herdr/comments/1w29y7v/>
- <https://www.reddit.com/r/herdr/comments/1w2pq9d/>
- <https://www.reddit.com/r/herdr/comments/1v5m3jb/>
- <https://www.reddit.com/r/herdr/comments/1v28abf/>
- <https://www.reddit.com/r/herdr/comments/1v9wweu/>
- <https://www.reddit.com/r/herdr/comments/1vkkzqp/>
- <https://www.reddit.com/r/herdr/comments/1w1mj6e/>
- <https://coles.codes/posts/herdr-vs-cmux/>

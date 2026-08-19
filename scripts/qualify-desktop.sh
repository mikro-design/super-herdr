#!/usr/bin/env bash
# Run the machine-checkable part of the manual desktop matrix and print a record.
#
# Most of the matrix needs a human at a desktop: dragging a selection, clicking
# a notification, pasting with the terminal's own command. The command-line
# claims around them do not, and re-checking those by hand on every desktop is
# how a matrix goes unrecorded. This runs those, on whatever host it is invoked
# from, and prints a record block to paste into TESTING.md.
#
# Real hostnames, addresses, and usernames never reach the output: targets are
# counted, not named, because the record is committed.
set -euo pipefail

binary="${SUPER_HERDR:-./target/release/super-herdr}"
config_args=()
if [[ $# -gt 0 ]]; then
  config_args=(--config "$1")
fi

if [[ ! -x "${binary}" ]]; then
  echo "no super-herdr binary at ${binary}; set SUPER_HERDR" >&2
  exit 2
fi

passes=0
failures=0
results=()

check() { # description, condition-already-evaluated exit status
  if [[ "$2" -eq 0 ]]; then
    results+=("PASS ${1}")
    passes=$((passes + 1))
  else
    results+=("FAIL ${1}")
    failures=$((failures + 1))
  fi
}

# A pty is the only way to see what an interactive operator would see.
run_pty() {
  if [[ "$(uname -s)" == "Darwin" ]]; then
    script -q /dev/null /bin/sh -c "$1" 2>/dev/null || true
  else
    script -qec "$1" /dev/null 2>/dev/null || true
  fi
}

has_ansi() { grep -q $'\x1b\[' "$1"; }

work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT

# 1. Clipboard capabilities, reported without reading a payload.
"${binary}" clipboard check > "${work}/clipboard" 2>&1 && status=0 || status=$?
check "clipboard check reports capabilities" "${status}"
clipboard_context="$(sed -n 's/^context: //p' "${work}/clipboard" | head -1)"
clipboard_copy="$(sed -n 's/^copy: //p' "${work}/clipboard" | head -1)"

# 2. Probe: failure isolation and output discipline.
"${binary}" "${config_args[@]}" probe > "${work}/probe" 2>&1 || true
"${binary}" "${config_args[@]}" probe --json > "${work}/json" 2>&1 || true
NO_COLOR=1 run_pty "${binary} ${config_args[*]} probe" > "${work}/nocolor"
run_pty "${binary} ${config_args[*]} probe" > "${work}/tty"

reachable="$(grep -c '^OK ' "${work}/probe" || true)"
unreachable="$(grep -c '^FAIL ' "${work}/probe" || true)"
targets=$((reachable + unreachable))

[[ "${reachable}" -ge 1 ]] && status=0 || status=1
check "at least one target reachable" "${status}"

if [[ "${unreachable}" -ge 1 ]]; then
  [[ "${reachable}" -ge 1 ]] && status=0 || status=1
  check "an unreachable target is isolated from live ones" "${status}"
else
  results+=("SKIP failure isolation (every configured target is reachable)")
fi

has_ansi "${work}/probe" && status=1 || status=0
check "redirected probe output has no ANSI escapes" "${status}"
has_ansi "${work}/json" && status=1 || status=0
check "probe --json has no ANSI escapes" "${status}"
has_ansi "${work}/nocolor" && status=1 || status=0
check "NO_COLOR=1 suppresses ANSI escapes on a tty" "${status}"

if [[ "${unreachable}" -ge 1 && "${reachable}" -ge 1 ]]; then
  grep -q $'\x1b\[32m' "${work}/tty" && grep -q $'\x1b\[31m' "${work}/tty" && status=0 || status=1
  check "interactive OK is green and FAIL is red" "${status}"
fi

# A consumer closing the pipe early must not turn into a panic.
pipe_output="$("${binary}" "${config_args[@]}" probe 2>&1 | head -1 || true)"
[[ "${pipe_output}" != *panic* ]] && status=0 || status=1
check "early pipe close exits without a panic" "${status}"

# 3. Native notification delivery, as this desktop actually reports it.
"${binary}" notifications check > "${work}/notify" 2>&1 && status=0 || status=$?
check "notifications check reports delivery" "${status}"
notify_delivery="$(sed -n 's/^delivery: //p' "${work}/notify" | head -1)"
notify_click="$(sed -n 's/^click to jump: //p' "${work}/notify" | head -1)"

# 4. The TUI itself starts, renders, and quits on its documented chord. Only
# Super-Herdr's own prefix is sent, which it intercepts, so nothing reaches a
# pane and no running process is disturbed.
tui_smoke() {
  ( sleep 5; printf '\035q' ) | run_pty "${binary} ${config_args[*]} tui" > "${work}/tui" &
  local pid=$!
  local waited=0
  while kill -0 "${pid}" 2> /dev/null && [[ "${waited}" -lt 25 ]]; do
    sleep 1
    waited=$((waited + 1))
  done
  if kill -0 "${pid}" 2> /dev/null; then
    kill "${pid}" 2> /dev/null || true
    return 1
  fi
  wait "${pid}"
}

tui_smoke && status=0 || status=$?
check "TUI renders and quits on Ctrl+] q" "${status}"
[[ -s "${work}/tui" ]] && ! grep -qi panic "${work}/tui" && status=0 || status=1
check "TUI frame rendered without a panic" "${status}"

herdr_version="$(sed -n 's/.*"herdr_version": "\([^"]*\)".*/\1/p' "${work}/json" | head -1)"
herdr_protocol="$(sed -n 's/.*"protocol": \([0-9]*\).*/\1/p' "${work}/json" | head -1)"

display="none"
if [[ -n "${WAYLAND_DISPLAY:-}" ]]; then
  display="wayland"
elif [[ -n "${DISPLAY:-}" ]]; then
  display="x11"
fi
tools=()
for tool in wl-copy xclip xsel pbcopy notify-send; do
  command -v "${tool}" > /dev/null 2>&1 && tools+=("${tool}")
done

printf '\n'
printf '%s\n' "${results[@]}"
printf '\n--- record block ---\n'
cat <<RECORD
- Host: $(uname -s) $(uname -r | cut -d- -f1) ${HOSTTYPE:-$(uname -m)}
- Terminal: ${TERM_PROGRAM:-unknown} (TERM=${TERM:-unset}), display protocol: ${display}
- Clipboard context: ${clipboard_context:-unknown}; copy path: ${clipboard_copy:-unknown}
- Clipboard tools present: ${tools[*]:-none}
- Notifications: delivery ${notify_delivery:-unknown}; click to jump ${notify_click:-unknown}
- Herdr ${herdr_version:-unknown} protocol ${herdr_protocol:-unknown}; ${targets} target(s), ${reachable} reachable
- Super-Herdr $(${binary} --version | awk '{print $2}') at commit $(git rev-parse --short HEAD 2>/dev/null || echo unknown)
- Automated checks: ${passes} passed, ${failures} failed
RECORD

[[ "${failures}" -eq 0 ]]

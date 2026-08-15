#!/opt/code-server/lib/node
'use strict';

// This executable is the only process profile available to VS Code's native
// terminal tools. It deliberately is not a shell: stdin and stdout are
// bridged through a 0600 Unix socket to the permission-checked Wings SSH
// terminal for the exact Web IDE session. The socket exists only in this
// sidecar's private runtime mount, so no node TCP service is exposed or made
// reachable from the container. Ordinary lines reach the application console,
// while `.wings` commands use Wings' normal power/stat dispatcher.
const net = require('node:net');

const SOCKET_PATH = '/run/jexactyl-webide/terminal.sock';
const MAX_PENDING_BYTES = 64 * 1024;
const COMMAND_IDLE_MS = 1_200;
// Wings power actions stop and start Docker asynchronously. Keep the native
// Execute process alive until the daemon reports the resulting state instead
// of closing after a quiet period while the restart is still in progress.
const POWER_COMMAND_IDLE_MS = 5_000;
const POWER_COMMAND_HARD_LIMIT_MS = 120_000;
const COMMAND_HARD_LIMIT_MS = 8_000;
const OSC = '\x1b]633;';
let explicitlyClosed = false;
let reconnectDelay = 250;
let socket;
let pending = Buffer.alloc(0);
let commandActive = false;
let commandIdleTimer;
let commandHardTimer;
let pendingCommandLine = '';
let activePowerAction;
let powerOutputBuffer = '';
let powerTransitionObserved = false;
let powerFallbackTimerArmed = false;
let previousInputWasCarriageReturn = false;
let stdinEnded = false;
let gracefulExitTimer;

function emit(value) {
  process.stdout.write(typeof value === 'string' ? value : Buffer.from(value));
}

function shellIntegration(value) {
  emit(`${OSC}${value}\x07`);
}

// OSC 633;E carries the command line to VS Code's rich command detector. The
// value is user input, so escape delimiters and control bytes before putting it
// in an OSC sequence; otherwise a command containing `;` could forge a second
// shell-integration field and corrupt the terminal result card.
function escapeOscValue(value) {
  let escaped = '';
  for (const character of value) {
    const codePoint = character.codePointAt(0);
    if (character === '\\') escaped += '\\\\';
    else if (character === ';') escaped += '\\x3b';
    else if (codePoint < 0x20 || codePoint === 0x7f) {
      escaped += `\\x${codePoint.toString(16).padStart(2, '0')}`;
    } else {
      escaped += character;
    }
  }
  return escaped;
}

function readyForCommand() {
  shellIntegration('P;HasRichCommandDetection=True');
  shellIntegration('A');
}

function finishCommand(exitCode = 0) {
  if (!commandActive) return;
  commandActive = false;
  clearTimeout(commandIdleTimer);
  clearTimeout(commandHardTimer);
  shellIntegration(`D;${exitCode}`);
  readyForCommand();
  activePowerAction = undefined;
  powerOutputBuffer = '';
  powerTransitionObserved = false;
  powerFallbackTimerArmed = false;
  exitAfterCommandIfNeeded();
}

function exitAfterCommandIfNeeded() {
  if (!stdinEnded || commandActive || gracefulExitTimer) return;
  gracefulExitTimer = setTimeout(() => {
    explicitlyClosed = true;
    if (socket && !socket.destroyed) {
      socket.end(() => process.exit(0));
      setTimeout(() => process.exit(0), 500).unref();
    } else {
      process.exit(0);
    }
  }, 100);
}

function powerActionFor(commandLine) {
  const match = /^\.wings\s+power\s+(start|restart|stop|kill)(?:\s|$)/i.exec(commandLine.trim());
  return match ? match[1].toLowerCase() : undefined;
}

function powerOutputIndicatesFailure(output) {
  return /(?:already online|already restarting|already offline|already stopped|missing the `control\.|unexpected error|unknown command|usage: \.wings power|currently offline)/i.test(output);
}

function observePowerOutput(value) {
  if (!activePowerAction) return;
  // This bounded suffix is detection metadata only; the complete output is
  // still streamed to the native Execute terminal unchanged.
  powerOutputBuffer = `${powerOutputBuffer}${value}`.slice(-16 * 1024);
  const output = powerOutputBuffer;
  if (powerOutputIndicatesFailure(output)) {
    powerFallbackTimerArmed = true;
    scheduleCommandCompletion();
    return;
  }

  if (activePowerAction === 'restart') {
    // The connection prelude may contain "running" before the command is
    // accepted. Require a stopping/offline marker before accepting the final
    // running marker as proof that restart really occurred.
    if (/Server marked as (?:stopping|offline)\.\.\./i.test(output)) {
      powerTransitionObserved = true;
    }
    if (powerTransitionObserved && /Server marked as running\.\.\./i.test(output)) {
      finishCommand(0);
    }
    return;
  }

  if (activePowerAction === 'start') {
    if (/Server marked as running\.\.\./i.test(output)) finishCommand(0);
    return;
  }

  if (activePowerAction === 'stop' || activePowerAction === 'kill') {
    if (/Server marked as offline\.\.\./i.test(output)) finishCommand(0);
  }
}

function scheduleCommandCompletion() {
  if (!commandActive) return;
  clearTimeout(commandIdleTimer);
  // Power actions finish on an explicit Wings state marker. A short fallback
  // remains for immediate validation/permission errors with no transition.
  if (activePowerAction && !powerFallbackTimerArmed) return;
  commandIdleTimer = setTimeout(
    () => finishCommand(activePowerAction && !powerTransitionObserved ? 1 : 0),
    activePowerAction ? POWER_COMMAND_IDLE_MS : COMMAND_IDLE_MS,
  );
}

function beginCommand(commandLine = '') {
  if (commandActive) finishCommand(0);
  commandActive = true;
  activePowerAction = powerActionFor(commandLine);
  powerOutputBuffer = '';
  powerTransitionObserved = false;
  powerFallbackTimerArmed = false;
  // OSC 633;B marks the beginning of a command. It must be emitted for the
  // command that is actually about to be sent, not while the terminal is
  // merely sitting at a prompt. Emitting B from readyForCommand() leaves
  // VS Code's rich command detector with a phantom executing command. The
  // native Execute strategy then waits forever for the wrong completion
  // marker and eventually sends Ctrl-C/disposes the PTY before Wings sees
  // the real input.
  shellIntegration('B');
  shellIntegration(`E;${escapeOscValue(commandLine.slice(0, 1024))}`);
  shellIntegration('C');
  commandHardTimer = setTimeout(
    () => finishCommand(activePowerAction && !powerTransitionObserved ? 1 : 0),
    activePowerAction ? POWER_COMMAND_HARD_LIMIT_MS : COMMAND_HARD_LIMIT_MS,
  );
}

function sendInput(data) {
  if (!data) return;
  const input = Buffer.from(data);
  // Track the line sent to the PTY so the rich command detector can associate
  // the completion marker with the exact command. This is only metadata: the
  // original bytes are forwarded unchanged to Wings below.
  for (const character of input.toString('utf8')) {
    if (character === '\r' || character === '\n') {
      if (character === '\n' && previousInputWasCarriageReturn) {
        previousInputWasCarriageReturn = false;
        continue;
      }
      beginCommand(pendingCommandLine);
      pendingCommandLine = '';
      previousInputWasCarriageReturn = character === '\r';
    } else if (character === '\u0008' || character === '\u007f') {
      pendingCommandLine = pendingCommandLine.slice(0, -1);
      previousInputWasCarriageReturn = false;
    } else if (character >= ' ' && character !== '\u007f') {
      if (pendingCommandLine.length < 1024) pendingCommandLine += character;
      previousInputWasCarriageReturn = false;
    }
  }
  if (socket && !socket.destroyed && socket.writable) {
    socket.write(input);
    // Arm the quiet-period completion only after the command bytes have been
    // accepted by the Unix socket. Starting it in beginCommand races a slow
    // socket connection and can report success while the command is pending.
    if (commandActive) scheduleCommandCompletion();
    return;
  }
  pending = Buffer.concat([pending, input]).subarray(-MAX_PENDING_BYTES);
}

function connect() {
  if (explicitlyClosed) return;
  const current = net.createConnection({ path: SOCKET_PATH });
  socket = current;
  current.on('connect', () => {
    reconnectDelay = 250;
    if (pending.length) {
      current.write(pending);
      pending = Buffer.alloc(0);
      if (commandActive) scheduleCommandCompletion();
    }
  });
  current.on('data', (data) => {
    emit(data);
    observePowerOutput(data.toString('utf8'));
    scheduleCommandCompletion();
  });
  current.on('error', () => undefined);
  current.on('close', () => {
    if (socket === current) socket = undefined;
    if (explicitlyClosed) return;
    if (stdinEnded) {
      finishCommand(1);
      exitAfterCommandIfNeeded();
      return;
    }
    finishCommand(1);
    emit('\r\n[Jexactyl terminal disconnected; reconnecting\u2026]\r\n');
    const delay = reconnectDelay;
    reconnectDelay = Math.min(reconnectDelay * 2, 5_000);
    setTimeout(connect, delay);
  });
}

process.stdin.setEncoding('utf8');
process.stdin.on('data', sendInput);
process.stdin.on('end', () => {
  // Copilot closes the PTY's stdin immediately after sending a synchronous
  // command. Closing the Unix socket here races the pending write and was the
  // reason agent commands appeared successful without reaching Wings. Keep
  // the transport alive until the command has produced output (or hit its
  // bounded completion deadline), then exit cleanly with OSC 633 status.
  stdinEnded = true;
  exitAfterCommandIfNeeded();
});
for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP']) {
  process.on(signal, () => {
    explicitlyClosed = true;
    socket?.destroy();
    process.exit(0);
  });
}

readyForCommand();
connect();

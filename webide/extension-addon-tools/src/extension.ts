import * as vscode from 'vscode';

// The extension is bundled for the browser workbench, but its language-model
// tools execute in code-server's remote Node extension host. Keep the Node
// loader dynamic so the same bundle remains safe to load in a browser worker.
declare const require: ((moduleName: string) => unknown) | undefined;

type Resource = 'mods' | 'plugins';
type Source = 'modrinth' | 'curseforge' | 'spigot';

interface SearchInput {
    resource?: Resource;
    source?: Source;
    query?: string;
    page?: number;
    gameVersion?: string;
    modLoaderType?: number;
    loader?: string;
    sortField?: string;
    sortOrder?: 'asc' | 'desc';
    categoryId?: number;
    minRating?: number;
    platform?: string | string[];
}

interface InstallInput {
    resource?: Resource;
    source?: Source;
    ref?: string;
    projectId?: string | number;
    fileId?: string | number;
    gameVersion?: string;
    modLoaderType?: number;
    loader?: string;
    platform?: string | string[];
}

function settings() {
    const config = vscode.workspace.getConfiguration('jexactyl.webIde');
    return {
        endpoint: (config.get<string>('endpoint', '') || '').replace(/\/$/, ''),
        token: (config.get<string>('extensionToken', '') || '').slice(0, 128),
        socketPath: config.get<string>('addonToolsSocket', '') || '',
        enabled: config.get<boolean>('addonToolsEnabled', false),
    };
}

function requestUrl(endpoint: string): string {
    return `${endpoint.replace(/^ws:/, 'http:').replace(/^wss:/, 'https:')}/addon-tools`;
}

class LocalTransportError extends Error {}

function nodeNet(): { createConnection: (options: { path: string }) => any } | undefined {
    if (typeof require !== 'function') return undefined;
    try {
        return require('node:net') as { createConnection: (options: { path: string }) => any };
    } catch {
        return undefined;
    }
}

function validSocketPath(path: string): boolean {
    // This value is managed by Wings. Require the exact socket rather than a
    // directory prefix: pointing the addon at terminal.sock would otherwise
    // let a tampered setting inject JSON bytes into the server shell bridge.
    return path === '/run/jexactyl-webide/addon-tools.sock';
}

async function invokeLocalSocket(
    socketPath: string,
    operation: string,
    input: object,
    token?: vscode.CancellationToken,
): Promise<unknown> {
    const net = nodeNet();
    if (!net || !validSocketPath(socketPath)) throw new LocalTransportError('local addon socket unavailable');
    if (token?.isCancellationRequested) throw new LocalTransportError('cancelled');

    return new Promise((resolve, reject) => {
        let settled = false;
        let response = '';
        const socket = net.createConnection({ path: socketPath });
        const finish = (error?: Error, value?: unknown) => {
            if (settled) return;
            settled = true;
            try { socket.destroy(); } catch { /* already closed */ }
            if (error) reject(error); else resolve(value);
        };
        const cancellation = token?.onCancellationRequested(() =>
            finish(new LocalTransportError('cancelled')),
        );
        socket.setEncoding('utf8');
        socket.setTimeout(15_000, () => finish(new LocalTransportError('local addon socket timeout')));
        socket.once('connect', () => {
            try {
                socket.end(`${JSON.stringify({ operation, input })}\n`);
            } catch (error) {
                finish(error instanceof Error ? error : new LocalTransportError(String(error)));
            }
        });
        socket.on('data', (chunk: unknown) => {
            response += typeof chunk === 'string'
                ? chunk
                : new TextDecoder().decode(chunk as Uint8Array);
            if (response.length > 2 * 1024 * 1024) {
                finish(new LocalTransportError('local addon response exceeded the size limit'));
                return;
            }
            const newline = response.indexOf('\n');
            if (newline < 0) return;
            const line = response.slice(0, newline);
            try {
                const wrapper = JSON.parse(line) as { body?: unknown };
                if (!wrapper || typeof wrapper !== 'object' || !('body' in wrapper)) {
                    throw new Error('invalid local addon response');
                }
                cancellation?.dispose();
                finish(undefined, wrapper.body);
            } catch (error) {
                cancellation?.dispose();
                finish(new LocalTransportError(error instanceof Error ? error.message : String(error)));
            }
        });
        socket.once('error', (error: Error) => {
            cancellation?.dispose();
            finish(new LocalTransportError(error.message));
        });
        socket.once('close', () => {
            cancellation?.dispose();
            if (!settled) finish(new LocalTransportError('local addon socket closed without a response'));
        });
    });
}

async function invokeOperation(
    operation: 'status' | 'search' | 'install',
    input: object,
    token?: vscode.CancellationToken,
): Promise<unknown> {
    const config = settings();
    if (!config.enabled) {
        return { success: false, error_code: 'FEATURE_DISABLED', message: 'The administrator has disabled these tools for this server.' };
    }

    // Prefer the session-scoped Unix socket. It is mounted only into this
    // sidecar, so it works when the node's public URL is unreachable from the
    // Docker bridge and cannot be repurposed as an SSRF primitive.
    if (config.socketPath) {
        try {
            return await invokeLocalSocket(config.socketPath, operation, input, token);
        } catch (error) {
            if (!(error instanceof LocalTransportError)) {
                return { success: false, error_code: 'SESSION_UNAVAILABLE', message: 'The Web IDE session credential is unavailable.' };
            }
            if (error.message === 'cancelled') {
                return { success: false, error_code: 'CANCELLED', message: 'The addon operation was cancelled.' };
            }
            log('Local addon socket unavailable; trying the compatibility HTTPS path.', error);
        }
    }
    if (!config.endpoint || !config.token) {
        return { success: false, error_code: 'NODE_UNAVAILABLE', message: 'The Web IDE node could not be reached.' };
    }

    const controller = new AbortController();
    const cancellation = token?.onCancellationRequested(() => controller.abort());
    try {
        const response = await fetch(requestUrl(config.endpoint), {
            method: 'POST',
            cache: 'no-store',
            headers: { 'content-type': 'application/json', authorization: `Bearer ${config.token}` },
            body: JSON.stringify({ operation, input }),
            signal: controller.signal,
        });
        let body: unknown;
        try {
            body = await response.json();
        } catch {
            body = { success: false, error_code: 'INVALID_PANEL_RESPONSE', message: 'The panel returned an invalid response.' };
        }
        if (!response.ok && (typeof body !== 'object' || body === null)) {
            return { success: false, error_code: `HTTP_${response.status}`, message: 'The panel rejected this operation.' };
        }
        return body;
    } catch (error) {
        if (controller.signal.aborted) {
            return { success: false, error_code: 'CANCELLED', message: 'The addon operation was cancelled.' };
        }
        return { success: false, error_code: 'NODE_UNAVAILABLE', message: 'The Web IDE node could not be reached.' };
    } finally {
        cancellation?.dispose();
    }
}

async function invoke(operation: 'search' | 'install', input: object, token: vscode.CancellationToken): Promise<unknown> {
    return invokeOperation(operation, input, token);
}

function result(value: unknown): vscode.LanguageModelToolResult {
    return new vscode.LanguageModelToolResult([
        new vscode.LanguageModelTextPart(JSON.stringify(value)),
    ]);
}

let registrations: vscode.Disposable[] = [];
let policyTimer: ReturnType<typeof setInterval> | undefined;
let diagnostics: vscode.OutputChannel | undefined;
const TOOL_CONTEXT_KEY = 'jexactyl.webIde.addonToolsEnabled';

function updateToolContext(enabled: boolean): void {
    // The tool picker evaluates a normal context key, not the configuration
    // namespace itself.  Setting this explicitly also makes a managed
    // setting update visible to the picker without requiring a window reload.
    void vscode.commands.executeCommand('setContext', TOOL_CONTEXT_KEY, enabled).then(undefined, (error: unknown) => {
        log('Unable to update the tool-picker context.', error);
    });
}

function log(message: string, error?: unknown): void {
    const detail = error instanceof Error ? ` ${error.message}` : error ? ` ${String(error)}` : '';
    const line = `[Jexactyl Mods & Plugins] ${message}${detail}`;
    // Keep activation failures visible in both the extension-host log and the
    // normal VS Code Output panel.  Previously a missing/unsupported API was
    // silently ignored, making an installed-but-inert extension look like a
    // policy problem.
    if (error) {
        console.error(line, error);
    } else {
        console.info(line);
    }
    diagnostics?.appendLine(line);
}

async function refreshPolicy(context: vscode.ExtensionContext): Promise<void> {
    try {
        const body = await invokeOperation('status', {}) as { data?: { enabled?: unknown }; success?: boolean };
        if (body.data?.enabled === false) {
            log('The panel disabled the addon tool pack; unregistering tools.');
            registrations.forEach(disposable => disposable.dispose());
            registrations = [];
        } else if (body.data?.enabled === true && registrations.length === 0) {
            log('The panel enabled the addon tool pack; registering tools.');
            registerTools(context);
        }
    } catch (error) {
        // A transient panel/node failure should not remove tools. The next
        // presence poll or a tool invocation will retry authorization.
        log('Policy check failed; retaining the current registration.', error);
    }
}

function registerTools(context: vscode.ExtensionContext): void {
    registrations.forEach(disposable => disposable.dispose());
    registrations = [];
    if (policyTimer) clearInterval(policyTimer);
    policyTimer = undefined;
    const enabled = settings().enabled;
    updateToolContext(enabled);
    if (!enabled) {
        log('Addon tool pack is disabled by the managed session policy.');
        return;
    }
    // Older code-server/VS Code workbenches may not expose the language-model
    // tool registry. Keep this optional extension inert there instead of
    // allowing activation failure to affect the rest of the IDE.
    if (!vscode.lm || typeof vscode.lm.registerTool !== 'function') {
        log('This code-server extension host does not expose vscode.lm.registerTool.');
        return;
    }

    try {
        registrations.push(vscode.lm.registerTool<SearchInput>('jexactyl_mod_manager_search', {
            prepareInvocation: () => ({ invocationMessage: 'Searching the Jexactyl mod/plugin catalog…' }),
            invoke: async (options, token) => result(await invoke('search', options.input, token)),
        }));
        registrations.push(vscode.lm.registerTool<InstallInput>('jexactyl_mod_manager_install', {
            prepareInvocation: options => ({
                invocationMessage: 'Installing the selected Jexactyl addon…',
                confirmationMessages: {
                    title: 'Install this mod/plugin?',
                    message: `This will write the selected addon into the current server. ${JSON.stringify(options.input)}`,
                },
            }),
            invoke: async (options, token) => result(await invoke('install', options.input, token)),
        }));
    } catch (error) {
        registrations.forEach(disposable => disposable.dispose());
        registrations = [];
        log('Tool registration failed.', error);
        return;
    }
    log('Registered search and install tools.');
    context.subscriptions.push(...registrations);
    void refreshPolicy(context);
    policyTimer = setInterval(() => void refreshPolicy(context), 30_000);
}

export function activate(context: vscode.ExtensionContext): void {
    diagnostics = vscode.window.createOutputChannel('Jexactyl Mods & Plugins');
    context.subscriptions.push(diagnostics);
    log('Extension activated in the workspace host.');
    registerTools(context);
    context.subscriptions.push(vscode.workspace.onDidChangeConfiguration(event => {
        if (event.affectsConfiguration('jexactyl.webIde.addonToolsEnabled')) registerTools(context);
    }));
}

export function deactivate(): void {
    registrations.forEach(disposable => disposable.dispose());
    registrations = [];
    if (policyTimer) clearInterval(policyTimer);
    policyTimer = undefined;
    diagnostics = undefined;
}

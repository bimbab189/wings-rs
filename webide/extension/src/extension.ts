import * as vscode from 'vscode';
import * as Y from 'yjs';
import { WebsocketProvider } from 'y-websocket';

const LOCAL_ORIGIN = Symbol('jexactyl-local-editor');
const PALETTE = ['#4FC3F7', '#BA68C8', '#81C784', '#FFB74D', '#F06292', '#4DB6AC'];
const SERVER_TERMINAL_SHELL = '/opt/jexactyl/bin/jexactyl-terminal';

interface Binding {
    document: vscode.TextDocument;
    relativePath: string;
    ydoc: Y.Doc;
    text: Y.Text;
    provider: WebsocketProvider;
    applyingRemote: boolean;
    decorations: Map<number, vscode.TextEditorDecorationType>;
    disposables: vscode.Disposable[];
}

function configuration() {
    const config = vscode.workspace.getConfiguration('jexactyl.webIde');
    return {
        endpoint: config.get<string>('endpoint', '').replace(/\/$/, ''),
        // This is a session-scoped credential written by Wings into managed
        // settings.  Browser cookies are not reliably available to a web
        // extension worker, so use the credential explicitly for extension
        // requests while retaining cookies for normal browser requests.
        extensionToken: config.get<string>('extensionToken', '').slice(0, 128),
        displayName: config.get<string>('displayName', 'User').slice(0, 191),
        canUseConsole: config.get<boolean>('canUseConsole', false),
        byokProvider: config.get<'openai' | 'openrouter'>('byokProvider', 'openrouter'),
        byokModel: config.get<string>('byokModel', 'openai/gpt-4.1').slice(0, 191),
    };
}

function encodeRoom(value: string): string {
    const bytes = new TextEncoder().encode(value);
    let binary = '';
    bytes.forEach(byte => (binary += String.fromCharCode(byte)));
    return btoa(binary).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

function appendEndpoint(endpoint: string, suffix: string): string {
    return `${endpoint}/${suffix}`;
}

function authenticatedHeaders(): Record<string, string> {
    const token = configuration().extensionToken;
    return token ? { authorization: `Bearer ${token}` } : {};
}

function authenticatedProtocols(): string[] | undefined {
    const token = configuration().extensionToken;
    return token ? [`jexactyl-auth.${token}`] : undefined;
}

function authenticatedWebSocket(): typeof WebSocket {
    const token = configuration().extensionToken;
    if (!token) return WebSocket;
    // y-websocket constructs this value with `new WebSocket(url, protocols)`.
    // Force the short-lived session protocol so a worker that cannot access
    // the HttpOnly launch cookie still authenticates to Wings.
    return class AuthenticatedWebSocket extends WebSocket {
        constructor(url: string | URL, _protocols?: string | string[]) {
            super(url, [`jexactyl-auth.${token}`]);
        }
    } as typeof WebSocket;
}

let lastHeartbeat = 0;
function heartbeat(endpoint: string): void {
    const now = Date.now();
    if (!endpoint || now - lastHeartbeat < 10_000) return;
    lastHeartbeat = now;
    const url = appendEndpoint(endpoint.replace(/^ws:/, 'http:').replace(/^wss:/, 'https:'), 'heartbeat');
    void fetch(url, {
        method: 'POST',
        credentials: 'include',
        cache: 'no-store',
        headers: authenticatedHeaders(),
    }).catch(() => undefined);
}

function httpEndpoint(endpoint: string, suffix: string): string {
    return appendEndpoint(endpoint.replace(/^ws:/, 'http:').replace(/^wss:/, 'https:'), suffix);
}

function currentColorTheme(): string {
    return vscode.workspace.getConfiguration('workbench').get<string>('colorTheme', '').trim().slice(0, 256);
}

async function fetchUserTheme(): Promise<string | undefined> {
    const { endpoint } = configuration();
    if (!endpoint) return undefined;
    const response = await fetch(httpEndpoint(endpoint, 'profile/theme'), {
        method: 'GET',
        credentials: 'include',
        cache: 'no-store',
        headers: authenticatedHeaders(),
    });
    if (!response.ok) throw new Error(`Wings returned HTTP ${response.status} while reading the shared theme.`);
    const data = await response.json() as { theme?: unknown };
    return typeof data.theme === 'string' && data.theme.length <= 256 ? data.theme : undefined;
}

async function storeUserTheme(theme: string): Promise<void> {
    const { endpoint } = configuration();
    if (!endpoint || !theme || theme.length > 256 || /[\r\n\0]/.test(theme)) return;
    const response = await fetch(httpEndpoint(endpoint, 'profile/theme'), {
        method: 'POST',
        credentials: 'include',
        cache: 'no-store',
        headers: { 'content-type': 'application/json', ...authenticatedHeaders() },
        body: JSON.stringify({ theme }),
    });
    if (!response.ok) throw new Error(`Wings returned HTTP ${response.status} while storing the shared theme.`);
}

class ServerTerminal implements vscode.Pseudoterminal {
    private readonly writeEmitter = new vscode.EventEmitter<string>();
    private readonly closeEmitter = new vscode.EventEmitter<number>();
    private socket?: WebSocket;
    private connected = false;
    private readonly connectedWaiters = new Set<(error?: Error) => void>();
    private readonly outputListeners = new Set<(data: string) => void>();
    private inputLine = '';
    private pendingInput = '';
    private executionTimer?: ReturnType<typeof setTimeout>;
    private executionDeadline?: ReturnType<typeof setTimeout>;
    private executing = false;
    private explicitlyClosed = false;
    private connecting = false;
    private reconnectTimer?: ReturnType<typeof setTimeout>;
    private reconnectDelay = 1_000;
    private lastDimensions?: vscode.TerminalDimensions;
    readonly onDidWrite = this.writeEmitter.event;
    readonly onDidClose = this.closeEmitter.event;

    constructor(private readonly endpoint: string) {}

    private emit(data: string): void {
        this.writeEmitter.fire(data);
        for (const listener of this.outputListeners) listener(data);
    }

    private shellSequence(value: string): void {
        // This PTY carries a permission-checked Wings console, not a real
        // shell process.  OSC 633 is only meaningful to terminals with shell
        // integration enabled; code-server/xterm can display it literally,
        // which corrupts the console (and made selections appear to revert).
        // Keep the method as a no-op so the execution bookkeeping below stays
        // easy to audit without ever writing control sequences to the user.
        void value;
    }

    private finishExecution(): void {
        if (!this.executing) return;
        this.executing = false;
        if (this.executionTimer) clearTimeout(this.executionTimer);
        if (this.executionDeadline) clearTimeout(this.executionDeadline);
        this.executionTimer = undefined;
        this.executionDeadline = undefined;
    }

    private scheduleExecutionFinish(): void {
        if (!this.executing) return;
        if (this.executionTimer) clearTimeout(this.executionTimer);
        this.executionTimer = setTimeout(() => this.finishExecution(), 1_500);
    }

    private static escapeCommandLine(line: string): string {
        return line.replace(/\\/g, '\\\\').replace(/[\x00-\x20;]/g, character =>
            `\\x${character.charCodeAt(0).toString(16).padStart(2, '0')}`,
        );
    }

    private scheduleReconnect(): void {
        if (this.explicitlyClosed || this.reconnectTimer || this.connecting) return;
        const delay = this.reconnectDelay;
        this.reconnectDelay = Math.min(this.reconnectDelay * 2, 10_000);
        this.reconnectTimer = setTimeout(() => {
            this.reconnectTimer = undefined;
            this.connect();
        }, delay);
    }

    private connect(): void {
        if (this.explicitlyClosed || this.connecting || this.socket?.readyState === WebSocket.OPEN) return;
        this.connecting = true;
        const socket = new WebSocket(appendEndpoint(this.endpoint, 'terminal'), authenticatedProtocols());
        this.socket = socket;
        socket.binaryType = 'arraybuffer';
        socket.onopen = () => {
            this.connecting = false;
            this.connected = true;
            this.reconnectDelay = 1_000;
            for (const waiter of this.connectedWaiters) waiter();
            this.connectedWaiters.clear();
            if (this.pendingInput) {
                socket.send(JSON.stringify({ type: 'input', data: this.pendingInput }));
                this.pendingInput = '';
            }
            if (this.lastDimensions) this.setDimensions(this.lastDimensions);
        };
        socket.onmessage = event => {
            if (typeof event.data === 'string') this.emit(event.data);
            else if (event.data instanceof ArrayBuffer) this.emit(new TextDecoder().decode(event.data));
            this.scheduleExecutionFinish();
        };
        socket.onerror = () => this.emit('\r\n[Jexactyl terminal connection failed; retrying]\r\n');
        socket.onclose = () => {
            this.connected = false;
            this.connecting = false;
            if (this.socket === socket) this.socket = undefined;
            // A dropped WebSocket is recoverable.  Firing onDidClose here
            // tells VS Code that the terminal process exited and permanently
            // removes the tab, which was the cause of the disappearing
            // Jexactyl terminal during selection/reconnects.
            if (!this.explicitlyClosed) {
                this.emit('\r\n[Jexactyl terminal disconnected; reconnecting…]\r\n');
                this.scheduleReconnect();
            }
        };
    }

    open(dimensions?: vscode.TerminalDimensions): void {
        this.explicitlyClosed = false;
        if (dimensions) this.lastDimensions = dimensions;
        this.connect();
    }

    close(): void {
        this.explicitlyClosed = true;
        if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
        this.reconnectTimer = undefined;
        const socket = this.socket;
        this.socket = undefined;
        this.connected = false;
        this.connecting = false;
        for (const waiter of this.connectedWaiters) waiter(new Error('Wings terminal connection closed.'));
        this.connectedWaiters.clear();
        socket?.close(1000);
    }

    handleInput(data: string): void {
        if (data.length > 65_536) return;
        for (const character of data) {
            if (character === '\r' || character === '\n') {
                if (!this.executing) {
                    this.shellSequence('B');
                    this.shellSequence(`E;${ServerTerminal.escapeCommandLine(this.inputLine)}`);
                    this.shellSequence('C');
                    this.executing = true;
                    this.executionDeadline = setTimeout(() => this.finishExecution(), 30_000);
                }
                this.inputLine = '';
            } else if (character === '\u007f' || character === '\b') {
                this.inputLine = this.inputLine.slice(0, -1);
            } else if (character >= ' ' && this.inputLine.length < 1024) {
                this.inputLine += character;
            }
        }
        if (this.socket?.readyState !== WebSocket.OPEN) {
            this.pendingInput = `${this.pendingInput}${data}`.slice(-65_536);
            return;
        }
        this.socket.send(JSON.stringify({ type: 'input', data }));
        heartbeat(this.endpoint);
    }

    setDimensions(dimensions: vscode.TerminalDimensions): void {
        this.lastDimensions = dimensions;
        if (this.socket?.readyState !== WebSocket.OPEN) return;
        this.socket.send(JSON.stringify({ type: 'resize', cols: dimensions.columns, rows: dimensions.rows }));
    }

    private async waitUntilConnected(token?: vscode.CancellationToken): Promise<void> {
        if (this.connected && this.socket?.readyState === WebSocket.OPEN) return;
        await new Promise<void>((resolve, reject) => {
            let cancellation: vscode.Disposable | undefined;
            const timer = setTimeout(() => finish(new Error('Timed out connecting to the Wings terminal.')), 10_000);
            const finish = (error?: Error) => {
                clearTimeout(timer);
                cancellation?.dispose();
                this.connectedWaiters.delete(finish);
                if (error) reject(error);
                else resolve();
            };
            cancellation = token?.onCancellationRequested(() => finish(new Error('Command cancelled.')));
            this.connectedWaiters.add(finish);
        });
    }

    async executeLine(command: string, token?: vscode.CancellationToken): Promise<string> {
        const line = command.replace(/[\r\n\0]/g, '').slice(0, 1024).trim();
        if (!line) throw new Error('A non-empty single-line command is required.');
        await this.waitUntilConnected(token);
        return await new Promise<string>((resolve, reject) => {
            let output = '';
            let settled = false;
            let settleTimer: ReturnType<typeof setTimeout> | undefined;
            let cancellation: vscode.Disposable | undefined;
            const finish = (error?: Error) => {
                if (settled) return;
                settled = true;
                clearTimeout(timeout);
                if (settleTimer) clearTimeout(settleTimer);
                cancellation?.dispose();
                this.outputListeners.delete(collect);
                if (error) reject(error);
                else resolve(output.replace(/\x1b\[[0-?]*[ -\/]*[@-~]/g, '').slice(-32_768));
            };
            const collect = (data: string) => {
                output += data;
                if (settleTimer) clearTimeout(settleTimer);
                settleTimer = setTimeout(() => finish(), line.startsWith('.wings ') ? 1_500 : 2_000);
            };
            const timeout = setTimeout(() => finish(), 30_000);
            cancellation = token?.onCancellationRequested(() => finish(new Error('Command cancelled.')));
            this.outputListeners.add(collect);
            this.handleInput(`${line}\r`);
            // Wings returns command output as a stream rather than a shell
            // prompt. If a valid command is intentionally silent, there is no
            // output event to arm the normal quiet-period timer; use a bounded
            // fallback so an agent tool cannot wait forever. Any output keeps
            // extending the quiet-period timer above.
            settleTimer = setTimeout(() => finish(), line.startsWith('.wings ') ? 3_000 : 5_000);
        });
    }
}

class ServerConsole implements vscode.Pseudoterminal {
    private readonly writeEmitter = new vscode.EventEmitter<string>();
    private readonly closeEmitter = new vscode.EventEmitter<number>();
    private socket?: WebSocket;
    private input = '';
    private explicitlyClosed = false;
    private reconnectTimer?: ReturnType<typeof setTimeout>;
    private reconnectDelay = 1_000;
    readonly onDidWrite = this.writeEmitter.event;
    readonly onDidClose = this.closeEmitter.event;

    constructor(private readonly endpoint: string) {}

    private scheduleReconnect(): void {
        if (this.explicitlyClosed || this.reconnectTimer) return;
        const delay = this.reconnectDelay;
        this.reconnectDelay = Math.min(this.reconnectDelay * 2, 10_000);
        this.reconnectTimer = setTimeout(() => {
            this.reconnectTimer = undefined;
            this.connect();
        }, delay);
    }

    private connect(): void {
        if (this.explicitlyClosed || this.socket?.readyState === WebSocket.OPEN) return;
        const socket = new WebSocket(appendEndpoint(this.endpoint, 'console'), authenticatedProtocols());
        this.socket = socket;
        socket.onopen = () => {
            this.reconnectDelay = 1_000;
            this.writeEmitter.fire('[Jexactyl server console connected — press Enter to send a command]\r\n');
        };
        socket.onmessage = event => this.writeEmitter.fire(String(event.data).replace(/\n/g, '\r\n'));
        socket.onerror = () => this.writeEmitter.fire('\r\n[Console connection failed; retrying]\r\n');
        socket.onclose = () => {
            if (this.socket === socket) this.socket = undefined;
            if (!this.explicitlyClosed) {
                this.writeEmitter.fire('\r\n[Console disconnected; reconnecting…]\r\n');
                this.scheduleReconnect();
            }
        };
    }

    open(): void {
        this.explicitlyClosed = false;
        this.connect();
    }

    close(): void {
        this.explicitlyClosed = true;
        if (this.reconnectTimer) clearTimeout(this.reconnectTimer);
        this.reconnectTimer = undefined;
        const socket = this.socket;
        this.socket = undefined;
        socket?.close(1000);
    }

    handleInput(data: string): void {
        for (const character of data) {
            if (character === '\r' || character === '\n') {
                if (this.input && this.socket?.readyState === WebSocket.OPEN) {
                    this.socket.send(this.input.slice(0, 4096));
                    heartbeat(this.endpoint);
                }
                this.writeEmitter.fire('\r\n');
                this.input = '';
            } else if (character === '\u007f' || character === '\b') {
                if (this.input) {
                    this.input = this.input.slice(0, -1);
                    this.writeEmitter.fire('\b \b');
                }
            } else if (character >= ' ' && this.input.length < 4096) {
                this.input += character;
                this.writeEmitter.fire(character);
            }
        }
    }
}

let activeServerTerminal: { terminal: vscode.Terminal; pty: ServerTerminal } | undefined;

function isAllowedTerminal(terminal: vscode.Terminal): boolean {
    const options = terminal.creationOptions;
    // Pseudoterminals are created only by this extension. Keep both the
    // permission-checked Wings shell and the separately authorized daemon
    // console; neither has an OS process behind it.
    if ('pty' in options) {
        return terminal.name === 'Jexactyl Server' || terminal.name === 'Jexactyl Server Console';
    }
    const shellPath = (options as vscode.TerminalOptions).shellPath;
    if (shellPath) return shellPath === SERVER_TERMINAL_SHELL;
    // Restored terminals can omit shellPath. Only a terminal with our exact
    // name is allowed in that case; Bash, sh, debug and task terminals are
    // disposed as soon as they are observed.
    return terminal.name === 'Jexactyl Server';
}

function rejectNonServerTerminal(terminal: vscode.Terminal): void {
    if (isAllowedTerminal(terminal)) return;
    terminal.dispose();
}

function openServerTerminal(show = true): vscode.Terminal {
    if (activeServerTerminal) {
        if (show) activeServerTerminal.terminal.show();
        return activeServerTerminal.terminal;
    }
    const { endpoint } = configuration();
    const pty = new ServerTerminal(endpoint);
    const terminal = vscode.window.createTerminal({
        name: 'Jexactyl Server',
        pty,
        iconPath: new vscode.ThemeIcon('terminal'),
    });
    activeServerTerminal = { terminal, pty };
    if (show) terminal.show();
    return terminal;
}

async function executeServerLine(command: string, token?: vscode.CancellationToken): Promise<string> {
    openServerTerminal(true);
    return await activeServerTerminal!.pty.executeLine(command, token);
}

const BYOK_SECRET = 'jexactyl.webIde.byokApiKey';

async function configureByok(context: vscode.ExtensionContext): Promise<boolean> {
    const current = configuration();
    const provider = await vscode.window.showQuickPick(
        [
            { label: 'OpenRouter', value: 'openrouter' as const, detail: 'Use any OpenRouter model with your own key.' },
            { label: 'OpenAI', value: 'openai' as const, detail: 'Use an OpenAI model with your own key.' },
        ],
        { title: 'Jexactyl BYOK provider', placeHolder: 'The encrypted key is retained for this server and your panel user.' },
    );
    if (!provider) return false;
    const model = await vscode.window.showInputBox({
        title: 'Model identifier',
        value: provider.value === current.byokProvider ? current.byokModel : provider.value === 'openrouter' ? 'openai/gpt-4.1' : 'gpt-4.1',
        prompt: 'Enter a tool-capable model available to your provider account.',
        validateInput: value => /^[A-Za-z0-9._:/-]{1,191}$/.test(value) ? undefined : 'Use a valid provider model identifier.',
    });
    if (!model) return false;
    const key = await vscode.window.showInputBox({
        title: `${provider.label} API key`,
        password: true,
        ignoreFocusOut: true,
        prompt: 'The key is never written to the workspace, panel database, Wings logs, or provider request logs.',
        validateInput: value => value.length >= 8 && value.length <= 512 ? undefined : 'Enter a valid API key.',
    });
    if (!key) return false;
    const config = vscode.workspace.getConfiguration('jexactyl.webIde');
    await Promise.all([
        context.secrets.store(BYOK_SECRET, key),
        config.update('byokProvider', provider.value, vscode.ConfigurationTarget.Global),
        config.update('byokModel', model, vscode.ConfigurationTarget.Global),
    ]);
    vscode.window.showInformationMessage(`Jexactyl BYOK configured for ${model}.`);
    return true;
}

type OpenAIMessage = Record<string, unknown>;

function textFromParts(parts: readonly unknown[]): string {
    return parts
        .filter((part): part is vscode.LanguageModelTextPart => part instanceof vscode.LanguageModelTextPart)
        .map(part => part.value)
        .join('');
}

function convertMessages(messages: readonly vscode.LanguageModelChatRequestMessage[]): OpenAIMessage[] {
    const converted: OpenAIMessage[] = [];
    for (const message of messages) {
        const text = textFromParts(message.content);
        const calls = message.content.filter((part): part is vscode.LanguageModelToolCallPart => part instanceof vscode.LanguageModelToolCallPart);
        const results = message.content.filter((part): part is vscode.LanguageModelToolResultPart => part instanceof vscode.LanguageModelToolResultPart);
        if (message.role === vscode.LanguageModelChatMessageRole.Assistant) {
            converted.push({
                role: 'assistant',
                content: text || null,
                ...(calls.length ? {
                    tool_calls: calls.map(call => ({
                        id: call.callId,
                        type: 'function',
                        function: { name: call.name, arguments: JSON.stringify(call.input) },
                    })),
                } : {}),
            });
            continue;
        }
        if (text) converted.push({ role: 'user', content: text });
        for (const result of results) {
            converted.push({ role: 'tool', tool_call_id: result.callId, content: textFromParts(result.content) || 'Tool completed.' });
        }
    }
    return converted;
}

class ByokLanguageModelProvider implements vscode.LanguageModelChatProvider {
    private readonly changed = new vscode.EventEmitter<void>();
    readonly onDidChangeLanguageModelChatInformation = this.changed.event;

    constructor(private readonly context: vscode.ExtensionContext) {}

    refresh(): void {
        this.changed.fire();
    }

    async provideLanguageModelChatInformation(options: vscode.PrepareLanguageModelChatModelOptions): Promise<vscode.LanguageModelChatInformation[]> {
        let key = await this.context.secrets.get(BYOK_SECRET);
        if (!key && !options.silent) {
            await configureByok(this.context);
            key = await this.context.secrets.get(BYOK_SECRET);
        }
        if (!key) return [];
        const { byokProvider, byokModel } = configuration();
        return [{
            id: `${byokProvider}:${byokModel}`,
            name: byokModel,
            family: byokModel,
            version: 'byok',
            detail: byokProvider === 'openrouter' ? 'OpenRouter BYOK' : 'OpenAI BYOK',
            tooltip: 'Durable server/user BYOK model routed through the authenticated Wings endpoint.',
            maxInputTokens: 128_000,
            maxOutputTokens: 16_384,
            capabilities: { toolCalling: true, imageInput: false },
        }];
    }

    async provideLanguageModelChatResponse(
        _model: vscode.LanguageModelChatInformation,
        messages: readonly vscode.LanguageModelChatRequestMessage[],
        options: vscode.ProvideLanguageModelChatResponseOptions,
        progress: vscode.Progress<vscode.LanguageModelResponsePart>,
        token: vscode.CancellationToken,
    ): Promise<void> {
        const apiKey = await this.context.secrets.get(BYOK_SECRET);
        if (!apiKey) throw vscode.LanguageModelError.NoPermissions('Configure a BYOK key with “Jexactyl: Configure BYOK Agent”.');
        const { endpoint, byokProvider, byokModel } = configuration();
        const controller = new AbortController();
        const cancellation = token.onCancellationRequested(() => controller.abort());
        try {
            const headers = {
                'content-type': 'application/json',
                ...authenticatedHeaders(),
            };
            const response = await fetch(appendEndpoint(endpoint.replace(/^ws:/, 'http:').replace(/^wss:/, 'https:'), 'agent/chat'), {
                method: 'POST',
                credentials: 'include',
                cache: 'no-store',
                signal: controller.signal,
                headers,
                body: JSON.stringify({
                    provider: byokProvider,
                    api_key: apiKey,
                    model: byokModel,
                    messages: convertMessages(messages),
                    tools: options.tools?.map(tool => ({
                        type: 'function',
                        function: { name: tool.name, description: tool.description, parameters: tool.inputSchema || { type: 'object' } },
                    })),
                    tool_choice: options.toolMode === vscode.LanguageModelChatToolMode.Required ? 'required' : 'auto',
                }),
            });
            const data = await response.json() as {
                error?: { message?: string };
                choices?: Array<{ message?: { content?: string | null; tool_calls?: Array<{ id?: string; function?: { name?: string; arguments?: string } }> } }>;
            };
            if (!response.ok) throw new Error(data.error?.message || `Provider returned HTTP ${response.status}.`);
            const answer = data.choices?.[0]?.message;
            if (answer?.content) progress.report(new vscode.LanguageModelTextPart(answer.content));
            for (const call of answer?.tool_calls || []) {
                if (!call.id || !call.function?.name) continue;
                let input: object = {};
                try { input = JSON.parse(call.function.arguments || '{}') as object; } catch { input = {}; }
                progress.report(new vscode.LanguageModelToolCallPart(call.id, call.function.name, input));
            }
            if (!answer?.content && !answer?.tool_calls?.length) throw new Error('The provider returned an empty response.');
        } finally {
            cancellation.dispose();
        }
    }

    async provideTokenCount(_model: vscode.LanguageModelChatInformation, value: string | vscode.LanguageModelChatRequestMessage): Promise<number> {
        const text = typeof value === 'string' ? value : textFromParts(value.content);
        return Math.max(1, Math.ceil(text.length / 4));
    }
}

function replaceDocument(binding: Binding): void {
    const content = binding.text.toString();
    if (binding.document.getText() === content) return;
    const edit = new vscode.WorkspaceEdit();
    const end = binding.document.positionAt(binding.document.getText().length);
    edit.replace(binding.document.uri, new vscode.Range(new vscode.Position(0, 0), end), content);
    binding.applyingRemote = true;
    void Promise.resolve(vscode.workspace.applyEdit(edit)).finally(() => (binding.applyingRemote = false));
}

function updatePresence(binding: Binding): void {
    for (const decoration of binding.decorations.values()) decoration.dispose();
    binding.decorations.clear();
    for (const [clientId, state] of binding.provider.awareness.getStates()) {
        if (clientId === binding.ydoc.clientID || !state?.cursor || state.cursor.file !== binding.relativePath) continue;
        const color = PALETTE[Math.abs(Number(clientId)) % PALETTE.length];
        const name = String(state.user?.name || 'Collaborator').slice(0, 64);
        const selectionStart = binding.document.positionAt(Math.min(state.cursor.anchor, state.cursor.head));
        const selectionEnd = binding.document.positionAt(Math.max(state.cursor.anchor, state.cursor.head));
        const collapsed = state.cursor.anchor === state.cursor.head;
        const decoration = vscode.window.createTextEditorDecorationType({
            backgroundColor: collapsed ? undefined : `${color}3D`,
            borderColor: color,
            borderStyle: 'solid',
            borderWidth: collapsed ? '0 0 0 2px' : '0 0 2px 0',
            overviewRulerColor: color,
            overviewRulerLane: vscode.OverviewRulerLane.Right,
            after: {
                contentText: ` ${name} `,
                color: '#ffffff',
                backgroundColor: color,
                margin: '0 0 0 3px',
                border: `1px solid ${color}`,
            },
        });
        binding.decorations.set(clientId, decoration);
        const range = new vscode.Range(selectionStart, selectionEnd);
        for (const editor of vscode.window.visibleTextEditors.filter(editor => editor.document === binding.document)) {
            editor.setDecorations(decoration, [{ range, hoverMessage: `${name} is editing this file` }]);
        }
    }
}

export function activate(context: vscode.ExtensionContext): void {
    const bindings = new Map<string, Binding>();
    const status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Left, 10);
    status.name = 'Jexactyl Collaboration';
    status.text = '$(broadcast) Collaboration starting…';
    status.show();
    context.subscriptions.push(status);

    const bindDocument = (document: vscode.TextDocument): void => {
    const { endpoint, displayName } = configuration();
        if (!endpoint || document.uri.scheme !== 'file' || document.getText().length > 2 * 1024 * 1024) return;
        const relative = vscode.workspace.asRelativePath(document.uri, false);
        if (!relative || relative.startsWith('../') || relative === document.uri.fsPath) return;
        const key = document.uri.toString();
        if (bindings.has(key)) return;

        const ydoc = new Y.Doc();
        const text = ydoc.getText('content');
        const provider = new WebsocketProvider(appendEndpoint(endpoint, 'collaboration'), encodeRoom(relative), ydoc, {
            WebSocketPolyfill: authenticatedWebSocket(),
            connect: true,
        });
        const binding: Binding = { document, relativePath: relative, ydoc, text, provider, applyingRemote: false, decorations: new Map(), disposables: [] };
        bindings.set(key, binding);
        provider.awareness.setLocalStateField('user', { name: displayName });
        provider.on('sync', (synced: boolean) => {
            if (synced) replaceDocument(binding);
        });
        text.observe(event => {
            if (event.transaction.origin !== LOCAL_ORIGIN) replaceDocument(binding);
        });
        provider.awareness.on('change', () => {
            updatePresence(binding);
            const names = [...provider.awareness.getStates().values()]
                .map(state => String(state?.user?.name || 'User'))
                .filter((name, index, all) => all.indexOf(name) === index);
            status.text = `$(broadcast) ${names.length} online`;
            status.tooltip = names.join(', ');
        });
        provider.on('status', ({ status: connection }: { status: string }) => {
            status.text = connection === 'connected' ? '$(broadcast) Collaboration' : '$(debug-disconnect) Collaboration';
        });
    };

    const unbindDocument = (document: vscode.TextDocument): void => {
        const binding = bindings.get(document.uri.toString());
        if (!binding) return;
        for (const disposable of binding.disposables) disposable.dispose();
        for (const decoration of binding.decorations.values()) decoration.dispose();
        binding.provider.destroy();
        binding.ydoc.destroy();
        bindings.delete(document.uri.toString());
    };

    context.subscriptions.push(
        vscode.window.registerTerminalProfileProvider('jexactyl.serverTerminal', {
            provideTerminalProfile: () =>
                new vscode.TerminalProfile({
                    name: 'Jexactyl Server',
                    pty: new ServerTerminal(configuration().endpoint),
                    iconPath: new vscode.ThemeIcon('terminal'),
                }),
        }),
        vscode.window.onDidOpenTerminal(rejectNonServerTerminal),
        vscode.window.onDidCloseTerminal(terminal => {
            if (activeServerTerminal?.terminal === terminal) activeServerTerminal = undefined;
        }),
        vscode.commands.registerCommand('jexactyl.webIde.openTerminal', () => openServerTerminal(true)),
        vscode.commands.registerCommand('jexactyl.webIde.openConsole', () => {
            const { endpoint, canUseConsole } = configuration();
            if (!canUseConsole) return void vscode.window.showErrorMessage('You do not have permission to use the server console.');
            vscode.window.createTerminal({ name: 'Jexactyl Server Console', pty: new ServerConsole(endpoint), iconPath: new vscode.ThemeIcon('server-process') }).show();
        }),
        vscode.workspace.onDidOpenTextDocument(bindDocument),
        vscode.workspace.onDidCloseTextDocument(unbindDocument),
        vscode.workspace.onDidChangeTextDocument(event => {
            const binding = bindings.get(event.document.uri.toString());
            if (!binding || binding.applyingRemote || !binding.provider.synced) return;
            binding.ydoc.transact(() => {
                for (const change of [...event.contentChanges].sort((a, b) => b.rangeOffset - a.rangeOffset)) {
                    if (change.rangeLength) binding.text.delete(change.rangeOffset, change.rangeLength);
                    if (change.text) binding.text.insert(change.rangeOffset, change.text);
                }
            }, LOCAL_ORIGIN);
            heartbeat(configuration().endpoint);
        }),
        vscode.window.onDidChangeTextEditorSelection(event => {
            const binding = bindings.get(event.textEditor.document.uri.toString());
            const selection = event.selections[0];
            if (!binding || !selection) return;
            binding.provider.awareness.setLocalStateField('cursor', {
                file: binding.relativePath,
                anchor: binding.document.offsetAt(selection.anchor),
                head: binding.document.offsetAt(selection.active),
            });
            heartbeat(configuration().endpoint);
        }),
        { dispose: () => [...bindings.values()].forEach(binding => unbindDocument(binding.document)) },
    );

    // Theme selection is a portable, encrypted user preference. VS Code does
    // not apply external IndexedDB colorThemeData changes to a running
    // workbench, so synchronize the actual `workbench.colorTheme` setting.
    // Chat/workspace history remains untouched and server-scoped.
    let lastRemoteTheme: string | undefined;
    let pendingLocalTheme: string | undefined;
    let themePollRunning = false;
    let themePushTimer: ReturnType<typeof setTimeout> | undefined;
    let themeLiveSocket: WebSocket | undefined;
    let themeLiveReconnect: ReturnType<typeof setTimeout> | undefined;
    let themeSyncDisposed = false;
    const applyRemoteTheme = async (remote: string): Promise<void> => {
        if (!remote || remote.length > 256 || /[\r\n\0]/.test(remote) || pendingLocalTheme) return;
        lastRemoteTheme = remote;
        if (currentColorTheme() !== remote) {
            await vscode.workspace
                .getConfiguration('workbench')
                .update('colorTheme', remote, vscode.ConfigurationTarget.Global);
        }
    };
    const pullTheme = async (): Promise<void> => {
        if (themePollRunning || pendingLocalTheme) return;
        themePollRunning = true;
        try {
            const remote = await fetchUserTheme();
            if (pendingLocalTheme) return;
            if (!remote) {
                const local = currentColorTheme();
                if (local) {
                    await storeUserTheme(local);
                    lastRemoteTheme = local;
                }
                return;
            }
            await applyRemoteTheme(remote);
        } catch (error) {
            console.error('Jexactyl shared theme pull failed', error);
        } finally {
            themePollRunning = false;
        }
    };
    const queueThemePush = (): void => {
        const local = currentColorTheme();
        if (!local || local === lastRemoteTheme) return;
        pendingLocalTheme = local;
        if (themePushTimer) clearTimeout(themePushTimer);
        themePushTimer = setTimeout(() => {
            themePushTimer = undefined;
            const theme = pendingLocalTheme;
            if (!theme) return;
            void storeUserTheme(theme)
                .then(() => {
                    lastRemoteTheme = theme;
                    if (pendingLocalTheme === theme) pendingLocalTheme = undefined;
                })
                .catch(error => {
                    console.error('Jexactyl shared theme push failed', error);
                    if (pendingLocalTheme === theme) pendingLocalTheme = undefined;
                });
        }, 150);
    };
    const connectThemeEvents = (): void => {
        if (themeSyncDisposed || themeLiveSocket) return;
        const { endpoint } = configuration();
        if (!endpoint) return;
        const socket = new WebSocket(appendEndpoint(endpoint, 'profile/theme/live'), authenticatedProtocols());
        themeLiveSocket = socket;
        socket.onmessage = event => {
            if (typeof event.data !== 'string') return;
            try {
                const payload = JSON.parse(event.data) as { theme?: unknown };
                if (typeof payload.theme === 'string') {
                    void applyRemoteTheme(payload.theme).catch(error =>
                        console.error('Jexactyl live theme apply failed', error),
                    );
                }
            } catch (error) {
                console.error('Jexactyl live theme event was invalid', error);
            }
        };
        socket.onerror = () => undefined;
        socket.onclose = () => {
            if (themeLiveSocket === socket) themeLiveSocket = undefined;
            if (!themeSyncDisposed && !themeLiveReconnect) {
                themeLiveReconnect = setTimeout(() => {
                    themeLiveReconnect = undefined;
                    connectThemeEvents();
                }, 2_000);
            }
        };
    };
    context.subscriptions.push(
        vscode.workspace.onDidChangeConfiguration(event => {
            if (event.affectsConfiguration('workbench.colorTheme')) queueThemePush();
        }),
    );
    const initialThemeTimer = setTimeout(() => void pullTheme(), 500);
    connectThemeEvents();
    const themePollTimer = setInterval(() => void pullTheme(), 2_000);
    context.subscriptions.push({
        dispose: () => {
            themeSyncDisposed = true;
            clearTimeout(initialThemeTimer);
            clearInterval(themePollTimer);
            if (themePushTimer) clearTimeout(themePushTimer);
            if (themeLiveReconnect) clearTimeout(themeLiveReconnect);
            themeLiveSocket?.close(1000);
        },
    });
    // A previous session may have persisted a process terminal before the
    // profile policy was installed. Remove it before exposing the workspace.
    vscode.window.terminals.forEach(rejectNonServerTerminal);
    vscode.workspace.textDocuments.forEach(bindDocument);
    heartbeat(configuration().endpoint);
    // A browser tab can close without running extension deactivation hooks.
    // Renew a small authenticated presence lease instead of relying on user
    // edits or terminal input; Wings reaps the sidecar when these stop.
    const presenceTimer = setInterval(() => heartbeat(configuration().endpoint), 15_000);
    context.subscriptions.push({ dispose: () => clearInterval(presenceTimer) });

    // Make the permission-checked Wings terminal visible on first launch. This
    // remains available while the game container is offline so `.wings power
    // start` can be entered without first starting the server elsewhere.
    if (!vscode.window.terminals.some(terminal => terminal.name === 'Jexactyl Server')) {
        setTimeout(() => openServerTerminal(true), 750);
    }

    // Agent APIs are additive. A code-server build without the current VS Code
    // LM API must not prevent terminal or collaboration activation.
    try {
        const provider = new ByokLanguageModelProvider(context);
        context.subscriptions.push(
            vscode.commands.registerCommand('jexactyl.webIde.manageByok', async () => {
                if (await configureByok(context)) provider.refresh();
            }),
            vscode.lm.registerLanguageModelChatProvider('jexactyl-byok', provider),
        );
    } catch (error) {
        console.error('Jexactyl BYOK agent registration failed', error);
    }
}

export function deactivate(): void {}

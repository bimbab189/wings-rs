/*
 * Keep the pinned Copilot contribution manifest aligned with the Web IDE
 * threat model. VS Code's browser workbench does not provide a trusted way for
 * an extension-host tool to prove that an arbitrary URL was fetched by the
 * user's own device. The sidecar is network-isolated, so the safe behavior is
 * to omit the host-side fetch tool entirely. Browser automation tools are
 * omitted too if a future Copilot build contributes them.
 */
const fs = require('node:fs');

// Wings replaces these sentinels while proxying the authenticated, no-store
// workbench bundle. Binding the capability in JavaScript is reliable across
// code-server's redirects; its final workbench HTML is not always the root
// document that receives our defensive meta-tag fallback.
const storageScopePlaceholder = '__JEXACTYL_WEBIDE_STORAGE_SCOPE__';
const globalStorageScopePlaceholder = '__JEXACTYL_WEBIDE_GLOBAL_STORAGE_SCOPE__';
const storageApiPlaceholder = '__JEXACTYL_WEBIDE_STORAGE_API__';
const globalStorageApiPlaceholder = '__JEXACTYL_WEBIDE_GLOBAL_STORAGE_API__';
const storageTokenPlaceholder = '__JEXACTYL_WEBIDE_STORAGE_TOKEN__';

const manifestPath = '/opt/code-server/lib/vscode/extensions/copilot/package.json';
const bundlePath = '/opt/code-server/lib/vscode/extensions/copilot/dist/extension.js';
const manifest = JSON.parse(fs.readFileSync(manifestPath, 'utf8'));

// code-server's Open VSX marketplace does not publish GitHub Copilot Chat,
// although the pinned VS Code bundle contains the reviewed extension under
// its system extensions directory. Register that local copy as a built-in so
// GitHub sign-in resolves it in place instead of trying (and failing) to
// install `GitHub.copilot-chat` from the marketplace on every chat attempt.
const productPath = '/opt/code-server/lib/vscode/product.json';
const product = JSON.parse(fs.readFileSync(productPath, 'utf8'));
const builtIns = Array.isArray(product.builtInExtensions) ? product.builtInExtensions : [];
if (!builtIns.some((extension) => extension?.name?.toLowerCase() === 'github.copilot-chat')) {
  builtIns.push({
    name: 'GitHub.copilot-chat',
    version: manifest.version,
    repo: 'https://github.com/microsoft/vscode-copilot-chat',
    metadata: { publisherDisplayName: 'GitHub' },
  });
  product.builtInExtensions = builtIns;
  fs.writeFileSync(productPath, `${JSON.stringify(product, null, 2)}\n`, { mode: 0o644 });
}

// Browser VS Code normally disables persisted SecretStorage because its
// default browser encryption provider reports that encryption is unavailable.
// That makes GitHub/Copilot credentials disappear as soon as a Web IDE session
// is replaced. Install a small, version-pinned WebCrypto provider instead. It
// stores only an AES-GCM key in a per-user IndexedDB namespace; the
// values in VS Code's persisted storage are encrypted and never sent to Wings.
const legacyBrowserEncryption = 'var dpt=class{encrypt(i){return Promise.resolve(i)}decrypt(i){return Promise.resolve(i)}isEncryptionAvailable(){return Promise.resolve(!1)}getKeyStorageProvider(){return Promise.resolve("basic_text")}setUsePlainTextEncryption(){return Promise.resolve(void 0)}}';
const persistentBrowserEncryption = (className) => `var ${className}=class{
constructor(){this._keyPromise=void 0}
_scope(){try{return "${globalStorageScopePlaceholder}"||document.querySelector('meta[name="jexactyl-webide-global-scope"]')?.getAttribute("content")||location.host}catch{return location.host}}
_open(){const e="jexactyl-webide-secret-v1-"+encodeURIComponent(this._scope());return new Promise((t,o)=>{const n=indexedDB.open(e,1);n.onupgradeneeded=()=>{n.result.objectStoreNames.contains("key")||n.result.createObjectStore("key")};n.onsuccess=()=>t(n.result);n.onerror=()=>o(n.error)})}
_b64(e){let t="";for(const o of e)t+=String.fromCharCode(o);return btoa(t)}
_unb64(e){const t=atob(e),o=new Uint8Array(t.length);for(let n=0;n<t.length;n++)o[n]=t.charCodeAt(n);return o}
async _key(){if(this._keyPromise)return this._keyPromise;return this._keyPromise=(async()=>{try{const e=await this._open(),t=await new Promise((o,n)=>{const r=e.transaction("key","readonly").objectStore("key").get("value");r.onsuccess=()=>o(r.result);r.onerror=()=>n(r.error)});if(t)return await crypto.subtle.importKey("raw",t,{name:"AES-GCM"},!1,["encrypt","decrypt"]);const o=await crypto.subtle.generateKey({name:"AES-GCM",length:256},!0,["encrypt","decrypt"]),n=await crypto.subtle.exportKey("raw",o);await new Promise((r,s)=>{const a=e.transaction("key","readwrite").objectStore("key").put(new Uint8Array(n),"value");a.onsuccess=()=>r();a.onerror=()=>s(a.error)});return o}catch{return null}})(),this._keyPromise}
async encrypt(e){const t=await this._key();if(!t)return e;try{const o=crypto.getRandomValues(new Uint8Array(12)),n=new Uint8Array(await crypto.subtle.encrypt({name:"AES-GCM",iv:o},t,new TextEncoder().encode(e))),r=new Uint8Array(o.length+n.length);return r.set(o),r.set(n,o.length),"jex1."+this._b64(r)}catch{return e}}
async decrypt(e){if(typeof e!=="string"||!e.startsWith("jex1."))return e;const t=await this._key();if(!t)return e;try{const o=this._unb64(e.slice(5));return new TextDecoder().decode(await crypto.subtle.decrypt({name:"AES-GCM",iv:o.slice(0,12)},t,o.slice(12)))}catch{return e}}
isEncryptionAvailable(){return Promise.resolve(typeof indexedDB!=="undefined"&&!!globalThis.crypto?.subtle)}
getKeyStorageProvider(){return Promise.resolve("basic_text")}
setUsePlainTextEncryption(){return Promise.resolve(void 0)}}`;

// VS Code's browser SecretStorage normally persists only in the browser's
// IndexedDB. That is unreliable behind a per-launch base path and does not
// provide cross-device continuation. Route the already extension-namespaced
// secret keys to Wings' authenticated, user-scoped encrypted vault.
// The short-lived capability is injected only into the no-store workbench
// document and expires with the Web IDE session.
const legacyBrowserSecretStorage = (baseClass, sequencer) => `var ASe=class extends ${baseClass}{constructor(i,e,t,o){super(!0,i,e,o),t.options?.secretStorageProvider&&(this._secretStorageProvider=t.options.secretStorageProvider,this._embedderSequencer=new ${sequencer})}get(i){return this._secretStorageProvider?this._embedderSequencer.queue(i,()=>this._secretStorageProvider.get(i)):super.get(i)}set(i,e){return this._secretStorageProvider?this._embedderSequencer.queue(i,async()=>{await this._secretStorageProvider.set(i,e),this.onDidChangeSecretEmitter.fire(i)}):super.set(i,e)}delete(i){return this._secretStorageProvider?this._embedderSequencer.queue(i,async()=>{await this._secretStorageProvider.delete(i),this.onDidChangeSecretEmitter.fire(i)}):super.delete(i)}get type(){return this._secretStorageProvider?this._secretStorageProvider.type:super.type}keys(){if(this._secretStorageProvider){if(!this._secretStorageProvider.keys)throw new Error("Secret storage provider does not support keys() method");return this._secretStorageProvider.keys()}return super.keys()}}`;
const persistentBrowserSecretStorage = (className, baseClass, sequencer, args) => `var ${className}=class extends ${baseClass}{
constructor(${args.join(',')}){super(!0,${args[0]},${args[1]},${args[3]}),${args[2]}.options?.secretStorageProvider&&(this._secretStorageProvider=${args[2]}.options.secretStorageProvider,this._embedderSequencer=new ${sequencer});this._jexactylVault=this._vault(),this._jexactylVault&&(this._jexactylSequencer=new ${sequencer})}
_vault(){try{const i="${globalStorageApiPlaceholder}"||document.querySelector('meta[name="jexactyl-webide-global-storage-api"]')?.getAttribute("content"),e="${storageTokenPlaceholder}"||document.querySelector('meta[name="jexactyl-webide-storage-token"]')?.getAttribute("content");return i&&e?{api:i,token:e}:void 0}catch{return void 0}}
async _request(i){if(!this._jexactylVault)throw new Error("Jexactyl browser secret vault is unavailable");let e;for(let t=0;t<3;t++){try{const o=await fetch(this._jexactylVault.api,{method:"POST",credentials:"same-origin",cache:"no-store",headers:{"content-type":"application/json","x-jexactyl-browser-storage":this._jexactylVault.token},body:JSON.stringify(i)});if(!o.ok)throw new Error("Jexactyl browser secret vault returned "+o.status);return o.status===204?{}:await o.json()}catch(o){if(e=o,t===2)throw o;await new Promise(n=>setTimeout(n,150*(t+1)))}}throw e}
get(i){return this._secretStorageProvider?this._embedderSequencer.queue(i,()=>this._secretStorageProvider.get(i)):this._jexactylVault?this._jexactylSequencer.queue(i,async()=>(await this._request({operation:"secret_get",key:i})).value):super.get(i)}
set(i,e){return this._secretStorageProvider?this._embedderSequencer.queue(i,async()=>{await this._secretStorageProvider.set(i,e),this.onDidChangeSecretEmitter.fire(i)}):this._jexactylVault?this._jexactylSequencer.queue(i,async()=>{await this._request({operation:"secret_set",key:i,value:e}),this.onDidChangeSecretEmitter.fire(i)}):super.set(i,e)}
delete(i){return this._secretStorageProvider?this._embedderSequencer.queue(i,async()=>{await this._secretStorageProvider.delete(i),this.onDidChangeSecretEmitter.fire(i)}):this._jexactylVault?this._jexactylSequencer.queue(i,async()=>{await this._request({operation:"secret_delete",key:i}),this.onDidChangeSecretEmitter.fire(i)}):super.delete(i)}
get type(){return this._secretStorageProvider?this._secretStorageProvider.type:super.type}
keys(){if(this._secretStorageProvider){if(!this._secretStorageProvider.keys)throw new Error("Secret storage provider does not support keys() method");return this._secretStorageProvider.keys()}return this._jexactylVault?this._request({operation:"secret_keys"}).then(i=>i.keys||[]):super.keys()}}`;

// code-server supplies its own LocalStorageSecretStorageProvider to the
// browser workbench. That provider takes precedence over ASe above and was the
// reason successful GitHub sign-ins still disappeared on the next launch.
// Replace its backend with the same Wings vault. On the first upgraded launch
// it decrypts and migrates any still-readable legacy localStorage record, then
// removes that browser-only copy only after every record is durably written.
const persistentCodeServerSecretStorage = (className) => `${className}=class{
constructor(i){this.crypto=i;this.storageKey="secrets.provider";this._jexactylVault=this._vault();this.secretsPromise=this.load()}
_vault(){try{const i="${globalStorageApiPlaceholder}"||document.querySelector('meta[name="jexactyl-webide-global-storage-api"]')?.getAttribute("content"),e="${storageTokenPlaceholder}"||document.querySelector('meta[name="jexactyl-webide-storage-token"]')?.getAttribute("content");return i&&e?{api:i,token:e}:void 0}catch{return void 0}}
async _request(i){if(!this._jexactylVault)throw new Error("Jexactyl browser secret vault is unavailable");let e;for(let t=0;t<3;t++){try{const o=await fetch(this._jexactylVault.api,{method:"POST",credentials:"same-origin",cache:"no-store",headers:{"content-type":"application/json","x-jexactyl-browser-storage":this._jexactylVault.token},body:JSON.stringify(i)});if(!o.ok)throw new Error("Jexactyl browser secret vault returned "+o.status);return o.status===204?{}:await o.json()}catch(o){if(e=o,t===2)throw o;await new Promise(n=>setTimeout(n,150*(t+1)))}}throw e}
async load(){let i=this.loadAuthSessionFromElement(),e={};if(this._jexactylVault){const t=await this._request({operation:"secret_keys"}),o=Array.isArray(t.keys)?t.keys:[];for(const n of o){const r=await this._request({operation:"secret_get",key:n});typeof r.value==="string"&&(e[n]=r.value)}}let t=localStorage.getItem(this.storageKey);if(t)try{Object.assign(i,JSON.parse(await this.crypto.unseal(t)))}catch{}for(const[o,n]of Object.entries(i))if(e[o]===void 0){await this._request({operation:"secret_set",key:o,value:n});e[o]=n}return t&&this._jexactylVault&&localStorage.removeItem(this.storageKey),e}
loadAuthSessionFromElement(){let i,e=document.getElementById("vscode-workbench-auth-session"),t=e?e.getAttribute("data-settings"):void 0;if(t)try{i=JSON.parse(t)}catch{}if(!i)return{};let o={};if(o[\`\${Yi.urlProtocol}.loginAccount\`]=JSON.stringify(i),i.providerId!=="github")return o;let n=JSON.stringify({extensionId:"vscode.github-authentication",key:"github.auth"});return o[n]=JSON.stringify(i.scopes.map(r=>({id:i.id,scopes:r,accessToken:i.accessToken}))),o}
async get(i){return(await this.secretsPromise)[i]}
async set(i,e){let t=await this.secretsPromise;t[i]=e;await this._request({operation:"secret_set",key:i,value:e})}
async delete(i){let e=await this.secretsPromise;delete e[i];await this._request({operation:"secret_delete",key:i})}
async keys(){return Object.keys(await this.secretsPromise)}
async save(){let i=await this.secretsPromise;for(const[e,t]of Object.entries(i))await this._request({operation:"secret_set",key:e,value:t})}}`;

const forbidden = new Set([
  'copilot_fetchWebPage',
  'open_browser_page',
  'click_element',
  'screenshot_page',
  'navigate_page',
  'read_page',
  'hover_element',
  'drag_element',
  'type_in_page',
  'handle_dialog',
  'run_playwright_code',
]);

const contributions = manifest.contributes?.languageModelTools;
if (Array.isArray(contributions)) {
  manifest.contributes.languageModelTools = contributions.filter(
    (tool) => !forbidden.has(tool?.name),
  );
}

// The browser workbench must have the native tool registry before the user
// opens Chat's tool picker. Keep the extension's normal activation events, but
// add the standard eager event so the pinned package cannot remain invisible
// behind an unselected GitHub model in a code-server session.
if (Array.isArray(manifest.activationEvents) && !manifest.activationEvents.includes('*')) {
  manifest.activationEvents = [...manifest.activationEvents, '*'];
}

// Future package versions may express fetch as a normal contribution rather
// than a language-model tool. Never allow a contribution to opt back in.
for (const key of ['commands', 'menus', 'walkthroughs']) {
  if (!manifest.contributes?.[key]) continue;
  const serialized = JSON.stringify(manifest.contributes[key]);
  if (/(fetchWebPage|open_browser_page|run_playwright_code)/i.test(serialized)) {
    throw new Error(`forbidden browser contribution found in ${key}`);
  }
}

fs.writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`, { mode: 0o644 });

// The Copilot bundle registers the fetch implementation independently of its
// package contribution. Minified identifiers change between releases, so bind
// the removal to the implementation's reviewed semantic marker, derive its
// class identifier, and still require exactly one matching registration. This
// fails closed if upstream introduces a second implementation or changes the
// registration shape.
let bundle = fs.readFileSync(bundlePath, 'utf8');
const fetchMarker = 'static{this.toolName="fetch_webpage"}';
const fetchMarkers = bundle.split(fetchMarker).length - 1;
if (fetchMarkers !== 1) {
  throw new Error(`expected one Copilot fetch implementation, found ${fetchMarkers}`);
}
const fetchMarkerOffset = bundle.indexOf(fetchMarker);
const precedingClasses = [
  ...bundle
    .slice(Math.max(0, fetchMarkerOffset - 4096), fetchMarkerOffset)
    .matchAll(/([A-Za-z_$][\w$]*)=class\{/g),
];
if (!precedingClasses.length) {
  throw new Error('Copilot fetch implementation class was not found');
}
const fetchClass = precedingClasses.at(-1)[1];
const escapedFetchClass = fetchClass.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
const registrationPattern = new RegExp(
  `[A-Za-z_$][\\w$]*\\.registerTool\\(${escapedFetchClass}\\);`,
  'g',
);
const registrations = bundle.match(registrationPattern) ?? [];
if (registrations.length !== 1) {
  throw new Error(`expected one Copilot fetch registration, found ${registrations.length}`);
}
bundle = bundle.replace(
  registrationPattern,
  '/* Jexactyl: host-side fetch tool disabled. */',
);
fs.writeFileSync(
  bundlePath,
  bundle,
  { mode: 0o644 },
);

// The browser-side Web IDE must never expose a process-terminal profile from
// a bundled extension. The Jexactyl extension is the sole terminal provider;
// its PTY forwards to the permission-checked Wings shell. Removing the
// contribution points here is stronger than a user setting because settings
// can otherwise be edited or restored from a previous code-server profile.
for (const extensionName of ['ms-vscode.js-debug', 'copilot']) {
  const extensionManifestPath = `/opt/code-server/lib/vscode/extensions/${extensionName}/package.json`;
  const extensionManifest = JSON.parse(fs.readFileSync(extensionManifestPath, 'utf8'));
  if (!extensionManifest.contributes?.terminal) {
    throw new Error(`expected ${extensionName} to contribute a terminal profile`);
  }
  delete extensionManifest.contributes.terminal;
  fs.writeFileSync(extensionManifestPath, `${JSON.stringify(extensionManifest, null, 2)}\n`, { mode: 0o644 });
}

// VS Code also registers two core actions in the Terminal/Menubar and
// terminal title menus. They send editor text or a file path to the active
// process terminal, which is not a valid operation for the Wings PTY. Keep
// the command implementation for forward compatibility, but remove every
// visible menu registration from both browser workbench bundles. These
// version-pinned patterns intentionally fail the image build if upstream
// changes the menu shape, rather than silently reintroducing the actions.
const workbenchFiles = [
  '/opt/code-server/lib/vscode/out/vs/workbench/workbench.web.main.internal.js',
  '/opt/code-server/lib/vscode/out/vs/code/browser/workbench/workbench.js',
];
const menuPatterns = [
  {
    name: 'menubar run actions',
    expression: /,\{id:[A-Za-z0-9_$]+\.MenubarTerminalMenu,item:\{group:"3_run",command:\{id:"workbench\.action\.terminal\.(?:runActiveFile|runSelectedText)",title:[A-Za-z0-9_$]+\([^}]+?\)\},order:\d+,when:[^}]+?\}\}/g,
    expected: 2,
  },
  {
    name: 'terminal title run actions',
    expression: /,\{id:[A-Za-z0-9_$]+\.ViewTitle,item:\{command:\{id:"workbench\.action\.terminal\.(?:runActiveFile|runSelectedText)",title:[A-Za-z0-9_$]+\([^}]+?\),icon:[A-Za-z0-9_$]+\.[^}]+?\},group:"navigation",order:\d+,when:[^}]+?,isHiddenByDefault:!0\}\}/g,
    expected: 2,
  },
  {
    name: 'vscode-terminal title run actions',
    expression: /[A-Za-z0-9_$]+\.appendMenuItem\([A-Za-z0-9_$]+,\{command:\{id:"workbench\.action\.terminal\.(?:runActiveFile|runSelectedText)",title:[A-Za-z0-9_$]+\([^}]+?\),icon:[A-Za-z0-9_$]+\.[^}]+?\},group:"navigation",order:\d+,when:[^}]+?,isHiddenByDefault:!0\}\),/g,
    expected: 2,
  },
];
for (const workbenchPath of workbenchFiles) {
  let workbench = fs.readFileSync(workbenchPath, 'utf8');

  // Keep VS Code's native Execute tool UX and output plumbing, but bind its
  // process-backed PTY to our non-shell Wings connector. The user profile is
  // intentionally ignored here: changing a setting must never make the agent
  // fall back to Bash inside the sidecar.
  const copilotProfilePattern = /_getChatTerminalProfile\(([A-Za-z0-9_$]+)\)\{let ([A-Za-z0-9_$]+);switch\(\1\)\{/g;
  const copilotProfileMatches = workbench.match(copilotProfilePattern) ?? [];
  if (copilotProfileMatches.length !== 1) {
    throw new Error(`${workbenchPath}: expected one Copilot terminal profile resolver, found ${copilotProfileMatches.length}`);
  }
  workbench = workbench.replace(
    copilotProfilePattern,
    '_getChatTerminalProfile($1){return{path:"/opt/jexactyl/bin/jexactyl-terminal",profileName:"Jexactyl Server"};let $2;switch($1){',
  );

  // Upstream describes run_in_terminal as a general OS shell, which caused
  // models to generate `cd`, `java -jar`, pipelines, and other commands that
  // are meaningless (and unsafe) for a game/application console. Replace the
  // model-facing contract while retaining the native Execute tool identity,
  // category, confirmation UI, output capture, and companion terminal tools.
  const terminalRegistrationPattern = /this\._instantiationService\.invokeFunction\(([A-Za-z0-9_$]+)\)\.then\(([A-Za-z0-9_$]+)=>\{if\(/g;
  const terminalRegistrationMatches = workbench.match(terminalRegistrationPattern) ?? [];
  if (terminalRegistrationMatches.length !== 1) {
    throw new Error(`${workbenchPath}: expected one native terminal tool registration, found ${terminalRegistrationMatches.length}`);
  }
  const terminalDescription = [
    'Send exactly one command to this server\'s Jexactyl/Wings application terminal.',
    'This is not Bash or an operating-system shell.',
    'Ordinary commands are delivered verbatim to the game or application stdin exactly like typing in the Jexactyl server console.',
    'Commands beginning with .wings use the permission-checked Wings dispatcher.',
    'Use .wings help to list daemon commands; common commands are .wings stats and .wings power start, restart, stop, or kill.',
    'Never use cd, ls, shell operators, executables, scripts, package managers, or commands that manually start the application.',
    'The command must be one non-empty line and mode must be sync. Power actions may take up to 120 seconds while Wings replaces the container.',
  ].join(' ');
  workbench = workbench.replace(
    terminalRegistrationPattern,
    (match, factory, tool) => `this._instantiationService.invokeFunction(${factory}).then(${tool}=>{${tool}.modelDescription=${JSON.stringify(terminalDescription)};${tool}.inputSchema={type:"object",additionalProperties:false,properties:{command:{type:"string",minLength:1,maxLength:1024,description:"One direct game/application console command, or one permission-checked .wings command. Use .wings help for supported daemon actions. No newlines and no OS-shell syntax."},explanation:{type:"string",description:"Why this application-console command is needed."},goal:{type:"string",description:"What this application-console command should accomplish."},mode:{type:"string",enum:["sync"],default:"sync",description:"Always sync: wait for and return the application terminal response."},timeout:{type:"number",minimum:0,maximum:120000,default:120000,description:"Maximum wait for the streamed application response and completion marker in milliseconds. Power actions may need up to two minutes."}},required:["command","explanation","goal","mode"]};if(`,
  );

  // The upstream Execute tool only attaches `toolResultDetails` when a
  // command fails. The Wings connector is successful by design, so that left
  // the user-facing tool card showing only `Ran <command>` even though the
  // model received the output. Attach the same terminal embed for successful
  // commands as well; this is the renderer's supported output surface and
  // keeps the raw result visible alongside the command.
  const successfulToolResultDetails = 'toolResultDetails:mu?{input:I,output:[{type:"embed",isText:!0,value:un}],isError:!0}:void 0';
  const successfulToolResultDetailsMatches = workbench.split(successfulToolResultDetails).length - 1;
  if (successfulToolResultDetailsMatches !== 1) {
    throw new Error(`${workbenchPath}: expected one Execute tool result-details branch, found ${successfulToolResultDetailsMatches}`);
  }
  workbench = workbench.replace(
    successfulToolResultDetails,
    'toolResultDetails:typeof un==="string"&&un.length>0?{input:I,output:[{type:"embed",isText:!0,value:un}],isError:mu}:void 0',
  );

  const codeServerProviderStart = workbench.match(/([A-Za-z0-9_$]+)=class\{constructor\(([A-Za-z0-9_$]+)\)\{this\.crypto=\2;this\.storageKey="secrets\.provider";/);
  if (codeServerProviderStart) {
    const callbackMarker = 'static{this.REQUEST_ID=0}';
    const callbackOccurrences = workbench.split(callbackMarker).length - 1;
    if (callbackOccurrences !== 1) {
      throw new Error(`${workbenchPath}: expected one URL callback provider, found ${callbackOccurrences}`);
    }
    const start = codeServerProviderStart.index;
    const providerTail = workbench.slice(start);
    const nextClass = providerTail.match(/},[A-Za-z0-9_$]+=class [A-Za-z0-9_$]+(?: extends [A-Za-z0-9_$]+)?\{constructor/);
    if (start === undefined || !nextClass || nextClass.index === undefined) {
      throw new Error(`${workbenchPath}: code-server SecretStorage provider boundary was not found`);
    }
    const end = start + nextClass.index + 1;
    workbench = `${workbench.slice(0, start)}${persistentCodeServerSecretStorage(codeServerProviderStart[1])}${workbench.slice(end)}`;
    if (workbench.split(callbackMarker).length - 1 !== 1) {
      throw new Error(`${workbenchPath}: SecretStorage patch removed the URL callback provider`);
    }
  } else if (workbenchPath.endsWith('/vs/code/browser/workbench/workbench.js')) {
    throw new Error(`${workbenchPath}: code-server SecretStorage provider was not found`);
  }
  const secretMarker = workbench.indexOf('secretStorageProvider');
  const secretClassMatches = secretMarker < 0 ? [] : [...workbench.slice(Math.max(0, secretMarker - 4096), secretMarker).matchAll(/var ([A-Za-z0-9_$]+)=class extends ([A-Za-z0-9_$]+)\{/g)];
  const secretStorageIdentifiers = secretClassMatches.at(-1);
  if (!secretStorageIdentifiers) {
    throw new Error(`${workbenchPath}: browser SecretStorage identifiers were not found`);
  }
  const secretStart = Math.max(0, secretMarker - 4096) + secretStorageIdentifiers.index;
  const secretEndMarker = 'return super.keys()}}';
  const secretEndOffset = workbench.indexOf(secretEndMarker, secretMarker);
  const secretConstructor = workbench.slice(secretStart, secretMarker).match(/constructor\(([^)]*)\)/);
  const secretSequencer = workbench.slice(secretStart, secretMarker + 512).match(/_embedderSequencer=new ([A-Za-z0-9_$]+)/);
  if (secretEndOffset < 0 || !secretConstructor || !secretSequencer) {
    throw new Error(`${workbenchPath}: browser SecretStorage boundary was not found`);
  }
  const secretEnd = secretEndOffset + secretEndMarker.length;
  const secretArgs = secretConstructor[1].split(',');
  if (secretArgs.length !== 4) {
    throw new Error(`${workbenchPath}: browser SecretStorage constructor changed`);
  }
  workbench = `${workbench.slice(0, secretStart)}${persistentBrowserSecretStorage(secretStorageIdentifiers[1], secretStorageIdentifiers[2], secretSequencer[1], secretArgs)}${workbench.slice(secretEnd)}`;
  const encryptionMatch = workbench.match(/var ([A-Za-z0-9_$]+)=class\{encrypt\(([A-Za-z0-9_$]+)\)\{return Promise\.resolve\(\2\)\}decrypt\(([A-Za-z0-9_$]+)\)\{return Promise\.resolve\(\3\)\}isEncryptionAvailable\(\)\{return Promise\.resolve\(!1\)\}getKeyStorageProvider\(\)\{return Promise\.resolve\("basic_text"\)\}setUsePlainTextEncryption\(\)\{return Promise\.resolve\(void 0\)\}\}/);
  if (!encryptionMatch || encryptionMatch.index === undefined) {
    throw new Error(`${workbenchPath}: expected one browser encryption provider`);
  }
  workbench = `${workbench.slice(0, encryptionMatch.index)}${persistentBrowserEncryption(encryptionMatch[1])}${workbench.slice(encryptionMatch.index + encryptionMatch[0].length)}`;
  const workspaceStorageMatch = workbench.match(/([A-Za-z0-9_$]+)\(location\.pathname\.toString\(\)\.replace\(\/\\\/\$\/,""\)\)\.toString\(16\)/);
  if (!workspaceStorageMatch) {
    throw new Error(`${workbenchPath}: expected one workspace storage pathname`);
  }
  const workspaceStorageExpression = workspaceStorageMatch[0];
  const stableWorkspaceStorageExpression = `${workspaceStorageMatch[1]}(("${storageScopePlaceholder}"||document.querySelector('meta[name="jexactyl-webide-scope"]')?.getAttribute("content")||location.pathname.toString().replace(/\\/$/,""))).toString(16)`;
  workbench = workbench.replace(workspaceStorageExpression, stableWorkspaceStorageExpression);
  // Application, shared-profile, and workspace stores all use this prefix.
  // Scope the prefix too: otherwise a second panel user on the same browser
  // origin could observe global Chat/UI state left by the first user even
  // though their workspace database and SecretStorage were isolated.
  const storagePrefix = 'static{this.STORAGE_DATABASE_PREFIX="vscode-web-state-db-"}';
  const scopedStoragePrefix = `static{this.STORAGE_DATABASE_PREFIX="vscode-web-state-db-"+("${storageScopePlaceholder}"||document.querySelector('meta[name="jexactyl-webide-scope"]')?.getAttribute("content")||location.host)+"-";this.GLOBAL_STORAGE_DATABASE_PREFIX="vscode-web-state-db-"+("${globalStorageScopePlaceholder}"||document.querySelector('meta[name="jexactyl-webide-global-scope"]')?.getAttribute("content")||location.host)+"-"}`;
  const storagePrefixOccurrences = workbench.split(storagePrefix).length - 1;
  if (storagePrefixOccurrences !== 1) {
    throw new Error(`${workbenchPath}: expected one storage database prefix, found ${storagePrefixOccurrences}`);
  }
  workbench = workbench.replace(storagePrefix, scopedStoragePrefix);
  const storageNameMatch = workbench.match(/this\.name=`\$\{([A-Za-z0-9_$]+)\.STORAGE_DATABASE_PREFIX\}\$\{([A-Za-z0-9_$]+)\.id\}`/);
  if (!storageNameMatch) {
    throw new Error(`${workbenchPath}: expected one browser storage database name`);
  }
  const storageNameExpression = storageNameMatch[0];
  const storageClass = storageNameMatch[1];
  const scopedStorageNameExpression = `this.name=\`\${${storageNameMatch[2]}.id==="global"||${storageNameMatch[2]}.id.startsWith("global-")?${storageNameMatch[1]}.GLOBAL_STORAGE_DATABASE_PREFIX:${storageNameMatch[1]}.STORAGE_DATABASE_PREFIX}\${${storageNameMatch[2]}.id}\``;
  workbench = workbench.replace(storageNameExpression, scopedStorageNameExpression);
  // Mirror VS Code's browser storage databases into the same authenticated
  // Wings vault. Native Chat stores its JSONL transcript on the server but
  // keeps the session index in this database; restoring the index is what
  // makes previous conversations appear after a new session or on a new
  // browser. Server values are applied first and the merged local database is
  // uploaded, which also migrates state from the immediately previous image.
  const storageConnectMatch = workbench.match(/async connect\(\)\{try\{return await ([A-Za-z0-9_$]+)\.create\(this\.name,void 0,\[([A-Za-z0-9_$]+)\.STORAGE_OBJECT_STORE\]\)\}/);
  const storageConnector = storageConnectMatch?.[1];
  if (!storageConnector || storageConnectMatch[2] !== storageClass) {
    throw new Error(`${workbenchPath}: browser storage connector was not found`);
  }
  const storageConnect = `async connect(){try{return await ${storageConnector}.create(this.name,void 0,[${storageClass}.STORAGE_OBJECT_STORE])}`;
  const persistentStorageConnect = `
_jexGlobal(){const e=this.name;return e.endsWith("-global")||e.endsWith("-global-shared")||e.includes("-global-")}
_jexVault(){try{const e=this._jexGlobal()?"${globalStorageApiPlaceholder}"||document.querySelector('meta[name="jexactyl-webide-global-storage-api"]')?.getAttribute("content"):"${storageApiPlaceholder}"||document.querySelector('meta[name="jexactyl-webide-storage-api"]')?.getAttribute("content"),t="${storageTokenPlaceholder}"||document.querySelector('meta[name="jexactyl-webide-storage-token"]')?.getAttribute("content");return e&&t?{api:e,token:t}:void 0}catch{return void 0}}
async _jexRequest(e){const t=this._jexVault();if(!t)return{};const o=await fetch(t.api,{method:"POST",credentials:"same-origin",cache:"no-store",headers:{"content-type":"application/json","x-jexactyl-browser-storage":t.token},body:JSON.stringify(e)});if(!o.ok)throw new Error("Jexactyl browser state vault returned "+o.status);return o.status===204?{}:await o.json()}
async _jexReconcile(e,t,o){const n=new Map(t),r=await e.getKeyValues(${storageClass}.STORAGE_OBJECT_STORE,e=>typeof e==="string"),a=new Map,i=new Set;for(const[e,t]of n)r.get(e)!==t&&a.set(e,t);for(const e of r.keys())n.has(e)||i.add(e);if(!a.size&&!i.size)return!1;await e.runInTransaction(${storageClass}.STORAGE_OBJECT_STORE,"readwrite",e=>{const t=[];for(const[o,n]of a)t.push(e.put(n,o));for(const o of i)t.push(e.delete(o));return t}),o&&this._onDidChangeItemsExternal.fire({changed:a,deleted:i});return!0}
async _jexPoll(e){if(!this._jexGlobal()||this.pendingUpdate||this._jexPolling)return;this._jexPolling=!0;try{const t=await this._jexRequest({operation:"storage_snapshot",database:this.name}),o=Array.isArray(t.entries)?t.entries:[];await this._jexReconcile(e,o,!0)}catch(t){this.logService.error("Jexactyl live user state sync failed: "+t)}finally{this._jexPolling=!1}}
_jexStartSync(e){this._jexGlobal()&&!this._jexSyncTimer&&(this._jexSyncTimer=setInterval(()=>this._jexPoll(e),2e3))}
async _jexRestore(e){if(!this._jexVault())return;try{const t=await this._jexRequest({operation:"storage_snapshot",database:this.name}),o=Array.isArray(t.entries)?t.entries:[],d=t.present===!0;d&&await this._jexReconcile(e,o,!1);let n=await e.getKeyValues(${storageClass}.STORAGE_OBJECT_STORE,r=>typeof r==="string");if(!d&&!n.size&&indexedDB.databases){const r="vscode-web-state-db-"+("${storageScopePlaceholder}"||location.host)+"-",a="vscode-web-state-db-"+("${globalStorageScopePlaceholder}"||location.host)+"-",t="vscode-web-state-db-"+location.host+"-",i=this._jexGlobal()?a:r,l=this.name.startsWith(i)?this.name.slice(i.length):"",c=this._jexGlobal()?[r+l,t+l]:[t+l];for(const o of c){if(!o||o===this.name)continue;const i=(await indexedDB.databases()).some(e=>e.name===o);if(!i)continue;const t=await ${storageConnector}.create(o,void 0,[${storageClass}.STORAGE_OBJECT_STORE]),r=await t.getKeyValues(${storageClass}.STORAGE_OBJECT_STORE,e=>typeof e==="string");if(r.size){await e.runInTransaction(${storageClass}.STORAGE_OBJECT_STORE,"readwrite",e=>[...r].map(([t,o])=>e.put(o,t))),n=r;await t.close();break}await t.close()}}!d&&n.size&&await this._jexRequest({operation:"storage_update",database:this.name,insert:[...n],delete:[]})}catch(t){this.logService.error("Jexactyl browser state restore failed: "+t)} }
async connect(){try{let e=await ${storageConnector}.create(this.name,void 0,[${storageClass}.STORAGE_OBJECT_STORE]);return await this._jexRestore(e),this._jexStartSync(e),e}`;
  const storageConnectOccurrences = workbench.split(storageConnect).length - 1;
  if (storageConnectOccurrences !== 1) {
    throw new Error(`${workbenchPath}: expected one browser storage connect method, found ${storageConnectOccurrences}`);
  }
  workbench = workbench.replace(storageConnect, persistentStorageConnect);
  const storageUpdateVariants = [
    ['return s}),!0)}async optimize(){}async close()', 't', 'o'],
    ['return a}),!0)}async optimize(){}async close()', 't', 'i'],
  ];
  const storageUpdateVariant = storageUpdateVariants.find(([tail]) => workbench.split(tail).length - 1 === 1);
  if (!storageUpdateVariant) {
    throw new Error(`${workbenchPath}: expected one browser storage update method`);
  }
  const [storageUpdateTail, insertVariable, deleteVariable] = storageUpdateVariant;
  const persistentStorageUpdateTail = `return ${storageUpdateTail.slice(7, 9)}),await this._jexRequest({operation:"storage_update",database:this.name,insert:${insertVariable}?[...${insertVariable}]:[],delete:${deleteVariable}?[...${deleteVariable}]:[]}).catch(n=>this.logService.error("Jexactyl browser state update failed: "+n)),!0)}async optimize(){}async close(){this._jexSyncTimer&&(clearInterval(this._jexSyncTimer),this._jexSyncTimer=void 0);let e=await this.whenConnected;return await this.pendingUpdate,e.close()}async _jexOriginalClose()`;
  workbench = workbench.replace(storageUpdateTail, persistentStorageUpdateTail);
  const storageClear = `async clear(){await(await this.whenConnected).runInTransaction(${storageClass}.STORAGE_OBJECT_STORE,"readwrite",t=>t.clear())}}`;
  const persistentStorageClear = `async clear(){await(await this.whenConnected).runInTransaction(${storageClass}.STORAGE_OBJECT_STORE,"readwrite",t=>t.clear()),await this._jexRequest({operation:"storage_clear",database:this.name}).catch(t=>this.logService.error("Jexactyl browser state clear failed: "+t))}}`;
  const storageClearOccurrences = workbench.split(storageClear).length - 1;
  if (storageClearOccurrences !== 1) {
    throw new Error(`${workbenchPath}: expected one browser storage clear method, found ${storageClearOccurrences}`);
  }
  workbench = workbench.replace(storageClear, persistentStorageClear);
  for (const { name, expression, expected } of menuPatterns) {
    const matches = workbench.match(expression) ?? [];
    if (matches.length !== expected) {
      throw new Error(`${workbenchPath}: expected ${expected} ${name}, found ${matches.length}`);
    }
    workbench = workbench.replace(expression, '');
  }
  if (/workbench\.action\.terminal\.(?:runActiveFile|runSelectedText)/.test(workbench)) {
    // The action registrations remain intentionally internal, but no menu
    // object may retain either command ID after the replacements above.
    const visible = workbench.match(/(?:MenubarTerminalMenu|ViewTitle|appendMenuItem)[^\n]{0,400}workbench\.action\.terminal\.(?:runActiveFile|runSelectedText)/g);
    if (visible?.length) throw new Error(`${workbenchPath}: run action still has a visible menu registration`);
  }
  fs.writeFileSync(workbenchPath, workbench, { mode: 0o644 });
}

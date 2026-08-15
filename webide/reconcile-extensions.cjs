'use strict';

const fs = require('node:fs');
const path = require('node:path');

const immutableRoot = '/opt/jexactyl/extensions-ro';
const runtimeRoot = path.resolve(process.argv[2] || '/run/jexactyl-webide/extensions');
const allowedIds = new Set([
  'jexactyl.jexactyl-web-ide',
  'jexactyl.jexactyl-mod-manager-tools',
]);

function readJson(file, fallback) {
  try {
    return JSON.parse(fs.readFileSync(file, 'utf8'));
  } catch (error) {
    if (error && error.code === 'ENOENT') return fallback;
    // A corrupt code-server registry must not be able to suppress the pinned
    // security extension. Rebuild only the registry metadata; extension data
    // and user-installed extension directories remain untouched.
    return fallback;
  }
}

function writeJsonAtomic(file, value) {
  const temporary = `${file}.jexactyl-${process.pid}`;
  fs.writeFileSync(temporary, `${JSON.stringify(value)}\n`, { mode: 0o644 });
  fs.renameSync(temporary, file);
}

function extensionIdentity(directory) {
  const manifestPath = path.join(directory, 'package.json');
  const manifest = readJson(manifestPath, null);
  if (!manifest || typeof manifest.name !== 'string' || typeof manifest.publisher !== 'string') return null;
  const id = `${manifest.publisher}.${manifest.name}`.toLowerCase();
  if (!allowedIds.has(id) || typeof manifest.version !== 'string' || !manifest.version) return null;
  return { id, version: manifest.version, manifestPath };
}

fs.mkdirSync(runtimeRoot, { recursive: true, mode: 0o700 });

const desired = new Map();
for (const name of fs.readdirSync(immutableRoot)) {
  const source = path.join(immutableRoot, name);
  if (!fs.lstatSync(source).isDirectory()) continue;
  const identity = extensionIdentity(source);
  if (!identity) continue;
  if (desired.has(identity.id)) throw new Error(`duplicate immutable Jexactyl extension: ${identity.id}`);
  desired.set(identity.id, { ...identity, name, source });
}

if (desired.size !== allowedIds.size) {
  throw new Error(`expected ${allowedIds.size} immutable Jexactyl extensions, found ${desired.size}`);
}

// Delete only directories positively identified as one of our first-party
// extensions. A restored user profile may contain several previous versions.
for (const name of fs.readdirSync(runtimeRoot)) {
  const candidate = path.join(runtimeRoot, name);
  let identity;
  try {
    if (!fs.lstatSync(candidate).isDirectory()) continue;
    identity = extensionIdentity(candidate);
  } catch {
    continue;
  }
  if (identity && allowedIds.has(identity.id)) fs.rmSync(candidate, { recursive: true, force: true });
}

for (const extension of desired.values()) {
  const destination = path.join(runtimeRoot, extension.name);
  fs.cpSync(extension.source, destination, { recursive: true, force: true });
}

const obsoletePath = path.join(runtimeRoot, '.obsolete');
const obsolete = readJson(obsoletePath, {});
if (obsolete && typeof obsolete === 'object' && !Array.isArray(obsolete)) {
  for (const key of Object.keys(obsolete)) {
    const normalized = key.toLowerCase();
    if ([...allowedIds].some(id => normalized === id || normalized.startsWith(`${id}-`))) delete obsolete[key];
  }
  writeJsonAtomic(obsoletePath, obsolete);
}

const registryPath = path.join(runtimeRoot, 'extensions.json');
const existingRegistry = readJson(registryPath, []);
const registry = Array.isArray(existingRegistry)
  ? existingRegistry.filter(entry => !allowedIds.has(String(entry?.identifier?.id || '').toLowerCase()))
  : [];

for (const extension of desired.values()) {
  registry.push({
    identifier: { id: extension.id },
    version: extension.version,
    location: { $mid: 1, path: path.join(runtimeRoot, extension.name), scheme: 'file' },
    relativeLocation: extension.name,
  });
}
writeJsonAtomic(registryPath, registry);

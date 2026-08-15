---
name: Jexactyl addon compatibility
description: Verify the current server's exact Minecraft loader, version, and plugin platform before using Jexactyl addon tools.
---

When using the Jexactyl mod or plugin catalog tools, treat the current server's compatibility metadata as authoritative.

- Always search the Jexactyl catalog before installing an addon whenever possible.
- For every addon, confirm that the response reports an exact compatibility result and that the artifact's `game_version` matches the server's exact version whenever the provider reports one. For a mod, also require the server loader to match the artifact loader. The loader IDs are Forge 1, Cauldron 2, LiteLoader 3, Fabric 4, Quilt 5, and NeoForge 6.
- For a plugin, require the server platform (the plugin equivalent of a loader) to match the artifact's platform metadata, and check its exact game version when one is reported. Do not treat a Paper, Purpur, Spigot, Folia, Velocity, Waterfall, Sponge, or BungeeCord artifact as interchangeable without checking the reported platform.
- Never choose the first provider version, trust a filename/project title alone, or substitute `latest`, a nearby Minecraft version, or a different loader. A filename containing a loader or `mc` suffix is useful evidence but is not a compatibility check by itself.
- Pass the opaque `ref` returned by the compatible search result to the install tool. Do not invent a project/file ID or override the server-owned version/loader/platform.
- Even if the user supplies a project or file ID, search the current catalog first and use that response to verify compatibility; user-supplied IDs are not proof of the correct artifact.
- If the compatibility result is missing, not exact, ambiguous, or does not match the requested addon, do not install. Explain the mismatch and ask the user to correct the server version/loader or choose a compatible artifact.
- After any server version or loader change, perform the compatibility check again; never reuse an old result as proof for the new server state.

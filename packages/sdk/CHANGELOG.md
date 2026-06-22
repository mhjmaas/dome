# Changelog

## 0.4.2 (2026-04-05)

### Added

- `exposeHost` option in `StartOptions` for forwarding host ports to the guest via `host.dome.internal`. Format: `"HOST:GUEST"` or `"PORT"`.

## 0.4.0 (2026-03-13)

### Added

- `sandbox.mkdir(path, opts?)` creates directories. Recursive by default (creates parents).
- `sandbox.readDir(path)` lists directory contents. Returns `{ name, type, size }[]` where type is `"file"`, `"dir"`, or `"symlink"`.
- `sandbox.stat(path)` returns file metadata: `{ size, mode, mtime, isDir, isFile, isSymlink }`.
- `sandbox.remove(path, opts?)` deletes files and directories. Pass `{ recursive: true }` to remove non-empty directories.
- `sandbox.rename(oldPath, newPath)` moves or renames files and directories within the guest.
- `sandbox.copy(src, dst, opts?)` copies a file. Pass `{ recursive: true }` to copy directories.
- `sandbox.chmod(path, mode)` changes file permissions (e.g. `0o755`).
- `sandbox.exists(path)` returns `true` if the path exists, `false` otherwise.
- Exported types: `DirEntry`, `StatResult`, `MkdirOptions`, `RemoveOptions`, `CopyOptions`.
- Unit tests for all new filesystem operations.
- Integration tests for all new filesystem operations against a real VM.

These are native protocol operations over vsock, not wrappers over shell commands.

## 0.3.1 (2026-03-12)

### Added

- **`exec(command: string | string[])`** pass an array to execute argv directly with no shell interpretation. String form is unchanged (`sh -c`).
- **`spawn(command: string | string[])`** same array overload for spawn.
- **`ExecOptions.shell`** override the default shell for string commands (e.g. `{ shell: "/bin/bash" }`).
- **`SpawnOptions.shell`** same shell override for spawn.

### Fixed

- Networking via `allowNet` now works correctly (requires CLI 0.4.1).

## 0.3.0 (2026-03-11)

### Added

- **`sandbox.spawn(command, opts?)`** — stream stdout/stderr in real-time from long-running processes. Returns a `SandboxProcess` handle with `.on("stdout" | "stderr" | "exit")`, `.write()`, `.kill()`, `.exited`, and `.pid`.
- **`sandbox.watch(path, handler, opts?)`** — watch directories for file changes inside the guest VM using guest-side inotify. Detects creates, modifications, deletions, and renames. Recursive by default.
- **`SandboxProcess.write(data)`** — write to a spawned process's stdin.
- **`SandboxProcess.kill()`** — terminate a spawned process.
- **`SpawnOptions`** — `cwd` and `env` options for `spawn()`.
- **`WatchOptions`** — `recursive` option for `watch()`.
- **`FileChangeEvent`** type — `{ path, event }` where event is `"create" | "modify" | "delete" | "rename"`.
- Concurrent operations — multiple `spawn()`, `exec()`, and `watch()` calls run in parallel within the same VM.
- Unit tests for spawn, kill, watch, and concurrent operations (mock-based).
- Integration tests for streaming exec, kill, stdin, and file watching against a real VM.

### Changed

- Internal `DomeProcess` now dispatches JSON-RPC notifications (`output`, `exit`, `file_change`) to registered handlers, enabling multiplexed streaming from multiple processes.

## 0.2.0

### Added

- `secrets` option — inject secrets via MITM proxy with per-host scoping.
- `network.allow` option — restrict guest network access by domain.
- `ports` option — port forwarding (`"host:guest"`).
- `mounts` option — bind-mount host directories into the guest.

## 0.1.0

### Added

- Initial release.
- `Sandbox.start()` / `.stop()` — boot and teardown microVMs.
- `sandbox.exec(command)` — buffered command execution.
- `sandbox.readFile(path)` / `sandbox.writeFile(path, content)` — guest file I/O.
- `sandbox.checkpoint(name)` — save disk state for later restoration.

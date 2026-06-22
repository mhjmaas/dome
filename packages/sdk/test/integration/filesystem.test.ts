/**
 * Integration tests for filesystem operations against the real dome VM.
 *
 * Prerequisites:
 *   1. cargo build -p dome-guest --target aarch64-unknown-linux-musl --release
 *   2. ./scripts/prepare-rootfs.sh
 *   3. cargo build -p dome-cli
 *   4. codesign --entitlements dome.entitlements --force -s - target/debug/dome
 *
 * Run: bun test test/integration/filesystem.test.ts
 */

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { existsSync } from "node:fs";
import { resolve } from "node:path";
import { Sandbox } from "../../src/sandbox";

const REPO_ROOT = resolve(import.meta.dir, "../../../..");
const DOME_BIN = resolve(REPO_ROOT, "target/debug/dome");

const canRun = existsSync(DOME_BIN);

describe.if(canRun)("filesystem operations", () => {
	let sb: Sandbox;

	beforeAll(async () => {
		sb = await Sandbox.start({ domeBin: DOME_BIN });
	}, 60_000);

	afterAll(async () => {
		await sb?.stop();
	}, 15_000);

	test("mkdir creates directory", async () => {
		await sb.mkdir("/tmp/fs-test-dir");
		const s = await sb.stat("/tmp/fs-test-dir");
		expect(s.isDir).toBe(true);
	}, 30_000);

	test("mkdir recursive creates nested directories", async () => {
		await sb.mkdir("/tmp/fs-test-nested/a/b/c");
		const s = await sb.stat("/tmp/fs-test-nested/a/b/c");
		expect(s.isDir).toBe(true);
	}, 30_000);

	test("writeFile + readDir lists files", async () => {
		await sb.mkdir("/tmp/fs-test-readdir");
		await sb.writeFile("/tmp/fs-test-readdir/hello.txt", "hello");
		await sb.writeFile("/tmp/fs-test-readdir/world.txt", "world");
		await sb.mkdir("/tmp/fs-test-readdir/subdir");

		const entries = await sb.readDir("/tmp/fs-test-readdir");
		const names = entries.map((e) => e.name).sort();
		expect(names).toEqual(["hello.txt", "subdir", "world.txt"]);

		const dir = entries.find((e) => e.name === "subdir");
		expect(dir?.type).toBe("dir");

		const file = entries.find((e) => e.name === "hello.txt");
		expect(file?.type).toBe("file");
		expect(file?.size).toBe(5);
	}, 30_000);

	test("stat returns file metadata", async () => {
		await sb.writeFile("/tmp/fs-test-stat.txt", "test content");
		const s = await sb.stat("/tmp/fs-test-stat.txt");
		expect(s.size).toBe(12);
		expect(s.isFile).toBe(true);
		expect(s.isDir).toBe(false);
		expect(s.isSymlink).toBe(false);
		expect(s.mtime).toBeGreaterThan(0);
		expect(s.mode).toBeGreaterThan(0);
	}, 30_000);

	test("stat throws for nonexistent path", async () => {
		await expect(
			sb.stat("/tmp/fs-test-does-not-exist"),
		).rejects.toThrow();
	}, 30_000);

	test("copy duplicates a file", async () => {
		await sb.writeFile("/tmp/fs-test-copy-src.txt", "copy me");
		await sb.copy("/tmp/fs-test-copy-src.txt", "/tmp/fs-test-copy-dst.txt");
		const data = await sb.readFile("/tmp/fs-test-copy-dst.txt");
		expect(new TextDecoder().decode(data)).toBe("copy me");
	}, 30_000);

	test("copy recursive duplicates a directory", async () => {
		await sb.mkdir("/tmp/fs-test-copydir/sub");
		await sb.writeFile("/tmp/fs-test-copydir/a.txt", "aaa");
		await sb.writeFile("/tmp/fs-test-copydir/sub/b.txt", "bbb");

		await sb.copy("/tmp/fs-test-copydir", "/tmp/fs-test-copydir2", {
			recursive: true,
		});

		const a = await sb.readFile("/tmp/fs-test-copydir2/a.txt");
		expect(new TextDecoder().decode(a)).toBe("aaa");
		const b = await sb.readFile("/tmp/fs-test-copydir2/sub/b.txt");
		expect(new TextDecoder().decode(b)).toBe("bbb");
	}, 30_000);

	test("rename moves a file", async () => {
		await sb.writeFile("/tmp/fs-test-rename-old.txt", "move me");
		await sb.rename(
			"/tmp/fs-test-rename-old.txt",
			"/tmp/fs-test-rename-new.txt",
		);

		const data = await sb.readFile("/tmp/fs-test-rename-new.txt");
		expect(new TextDecoder().decode(data)).toBe("move me");

		// Original should no longer exist
		await expect(sb.stat("/tmp/fs-test-rename-old.txt")).rejects.toThrow();
	}, 30_000);

	test("chmod changes file permissions", async () => {
		await sb.writeFile("/tmp/fs-test-chmod.txt", "chmod me");
		await sb.chmod("/tmp/fs-test-chmod.txt", 0o755);
		const s = await sb.stat("/tmp/fs-test-chmod.txt");
		// Check the permission bits (lower 12 bits of mode)
		expect(s.mode & 0o777).toBe(0o755);
	}, 30_000);

	test("remove deletes a file", async () => {
		await sb.writeFile("/tmp/fs-test-rm.txt", "delete me");
		await sb.remove("/tmp/fs-test-rm.txt");
		await expect(sb.stat("/tmp/fs-test-rm.txt")).rejects.toThrow();
	}, 30_000);

	test("remove recursive deletes a directory tree", async () => {
		await sb.mkdir("/tmp/fs-test-rmdir/nested");
		await sb.writeFile("/tmp/fs-test-rmdir/nested/file.txt", "data");
		await sb.remove("/tmp/fs-test-rmdir", { recursive: true });
		await expect(sb.stat("/tmp/fs-test-rmdir")).rejects.toThrow();
	}, 30_000);

	test("exists returns true for existing path", async () => {
		await sb.writeFile("/tmp/fs-test-exists.txt", "hi");
		expect(await sb.exists("/tmp/fs-test-exists.txt")).toBe(true);
	}, 30_000);

	test("exists returns false for missing path", async () => {
		expect(await sb.exists("/tmp/fs-test-nope-nope")).toBe(false);
	}, 30_000);
});

describe.if(!canRun)("filesystem operations (skipped)", () => {
	test("dome binary not found", () => {
		console.log(
			"Build with: cargo build -p dome-cli && codesign --entitlements dome.entitlements --force -s - target/debug/dome",
		);
	});
});

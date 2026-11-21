// node spikes/pi_watch.mjs [axiopera binary] (Node 24+). No Pi install or model required.
import assert from "node:assert/strict";
import cp from "node:child_process";
import { randomUUID } from "node:crypto";
import { chmodSync, copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { syncBuiltinESMExports } from "node:module";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { setTimeout as sleep } from "node:timers/promises";
import { fileURLToPath } from "node:url";

const root = mkdtempSync(join(tmpdir(), "axiopera-pi-watch-"));
const binary = process.platform === "win32" ? "axiopera.exe" : "axiopera";
const ax = join(root, binary);
const home = join(root, "ax");
const info = join(home, "daemon", "info.json");
const savedSpawn = cp.spawn;
const env = {};
for (const key of ["PATH", "PATHEXT", "SystemRoot", "WINDIR", "ComSpec", "PROCESSOR_ARCHITECTURE"]) {
	if (process.env[key]) env[key] = process.env[key];
}
Object.assign(env, { HOME: root, USERPROFILE: root, APPDATA: root, LOCALAPPDATA: root, AXIOPERA_HOME: home });
const callbacks = {}, children = [], messages = [];
const caller = randomUUID(), other = randomUUID();

async function until(check, why, timeout = 15_000) {
	const deadline = Date.now() + timeout;
	while (Date.now() < deadline) {
		if (check()) return;
		await sleep(100);
	}
	throw new Error(why);
}
function exited(pid) {
	try {
		process.kill(pid, 0);
		// In a Linux container PID 1 may leave an exited orphan as a zombie.
		return process.platform === "linux" && /\) Z /.test(readFileSync(`/proc/${pid}/stat`, "utf8"));
	} catch (e) {
		return e.code === "ESRCH" || e.code === "ENOENT";
	}
}
function run(...args) {
	const result = cp.spawnSync(ax, args, { env, cwd: root, encoding: "utf8", timeout: 10000 });
	assert.equal(result.status, 0, result.stderr);
	return result.stdout;
}
function start(id = caller, mode = "tui") {
	callbacks.session_start({}, { mode, cwd: root, sessionManager: {
		getSessionId: () => id, getBranch: () => messages,
	} });
}
function failTask(owner, name) {
	run("--caller", "pi:" + owner, "spawn", "pi", "--tier", "full", "--name", name, "watch probe");
	return until(() => JSON.parse(run("--caller", "pi:" + owner, "--json", "status", name)
		.split("\n")[0]).data[0].status === "failed", "fixture did not produce its failure event");
}

try {
	const source = process.argv[2] || fileURLToPath(new URL("../target/debug/" + binary, import.meta.url));
	copyFileSync(resolve(source), ax);
	chmodSync(ax, 0o755);
	mkdirSync(join(home, "daemon"), { recursive: true });
	// Node rejects Pi's RPC switches. This creates a real task event without loading an agent or model.
	writeFileSync(join(home, "config.toml"), "[agents.pi]\npath = " + JSON.stringify(process.execPath) + "\n");
	process.env.AXIOPERA_HOME = home;
	cp.spawn = (command, args, options) => {
		assert.equal(command, "axiopera");
		assert.deepEqual(args.slice(-3), ["watch", "--if-running", "--until-stdin-closes"]);
		const child = savedSpawn(ax, args, { ...options, env });
		const record = { child, frames: 0, done: false };
		child.stdout.on("data", () => record.frames++);
		child.once("close", () => { record.done = true; });
		children.push(record);
		return child;
	};
	syncBuiltinESMExports();
	const extension = (await import("../plugins/pi/index.ts")).default;
	extension({
		on: (name, cb) => { callbacks[name] = cb; },
		sendMessage: (message, opts) => {
			assert.deepEqual(opts, { deliverAs: "steer", triggerTurn: true });
			messages.push({ type: "custom_message", ...message });
		},
	});

	writeFileSync(info, JSON.stringify({ pid: process.pid }));
	process.env.AXIOPERA_TASK_ID = "managed-child";
	start();
	delete process.env.AXIOPERA_TASK_ID;
	start(caller, "rpc");
	start(caller, "print");
	assert.equal(children.length, 0);
	rmSync(info);
	start();
	assert.equal(children.length, 0);
	const dead = cp.spawnSync(process.execPath, ["-e", ""], { env }).pid;
	writeFileSync(info, JSON.stringify({ pid: dead }));
	await sleep(2300);
	assert.equal(children.length, 0);
	writeFileSync(info, JSON.stringify({ pid: process.pid }));
	await until(() => children.filter(c => c.done).length >= 2, "no retry while the endpoint was unavailable");
	assert.ok(!existsSync(join(home, "daemon", "daemon.log")), "watch started the daemon");
	callbacks.session_shutdown();

	await failTask(other, "foreign");
	start();
	await sleep(600);
	assert.equal(messages.length, 0);
	await failTask(caller, "own");
	await until(() => messages.length === 1, "owner missed its task event");
	const first = children.at(-1);
	const seq = messages[0].details.seq;
	first.child.stdin.end();
	await until(() => first.done && children.at(-1) !== first && children.at(-1).frames > 0, "disconnected watch did not replay");
	assert.equal(messages.length, 1);
	// Replay an older entry at the tail of the branch to exercise max-sequence recovery.
	messages.push({ ...messages[0], details: { caller: "pi:" + caller, seq: seq - 1 } });
	start();
	await until(() => children.at(-1).frames > 0, "reloaded watch received no replay");
	assert.equal(messages.length, 2);
	start(other);
	await until(() => messages.length === 3, "session switch missed the new owner's event");
	assert.equal(messages[2].details.caller, "pi:" + other);
	const last = children.at(-1);
	last.child.kill();
	await until(() => last.done, "watch did not exit");
	callbacks.session_shutdown();
	const stopped = children.length;
	await sleep(1200);
	assert.equal(children.length, stopped, "shutdown left a retry scheduled");

	const abrupt = cp.spawnSync(process.execPath, ["-e", `
		const w = require("node:child_process").spawn(process.argv[1],
			["watch", "--if-running", "--until-stdin-closes"], { stdio: ["pipe", "ignore", "ignore"], windowsHide: true });
		setTimeout(() => { process.kill(w.pid, 0); require("node:fs").writeSync(1, String(w.pid)); process.kill(process.pid, "SIGKILL"); }, 500);
	`, ax], { cwd: root, env, encoding: "utf8", timeout: 8000 });
	const pid = Number(abrupt.stdout);
	assert.ok(Number.isInteger(pid) && pid > 0, "child watch was not running before its parent died");
	await until(() => exited(pid), "watch survived its parent's abrupt exit");

	const originalDaemon = JSON.parse(readFileSync(info, "utf8")).pid;
	run("daemon", "stop", "--force");
	await until(() => exited(originalDaemon), "original daemon did not stop");
	const shortDaemon = savedSpawn(ax, ["daemon", "run", "--home", home, "--idle-secs", "1"], {
		cwd: root, env, stdio: "ignore", windowsHide: true,
	});
	await until(() => {
		try { return JSON.parse(readFileSync(info, "utf8")).pid === shortDaemon.pid; }
		catch { return false; }
	}, "short-lived daemon did not start");
	start(caller);
	const passive = children.at(-1);
	await until(() => passive.frames > 0 && !passive.done, "passive watch did not connect");
	const beforeIdle = messages.length;
	// The watchdog checks every 15 seconds. Querying the daemon here would reset its idle timer.
	await until(() => shortDaemon.exitCode !== null && passive.done, "passive watch kept the daemon alive", 35_000);
	assert.equal(shortDaemon.exitCode, 0);
	assert.match(readFileSync(join(home, "daemon", "daemon.log"), "utf8"), /idle, exiting/);
	await failTask(caller, "after-restart");
	await until(() => messages.length > beforeIdle, "watch missed the first event after an idle restart");
	assert.equal(messages.at(-1).details.caller, "pi:" + caller);
	assert.ok(messages.at(-1).details.seq > seq);
	console.log("Pi watcher passed startup retry, replay, reload, isolation, parent death, idle exit and restart delivery");
} finally {
	callbacks.session_shutdown?.();
	for (const { child, done } of children) if (!done) child.kill();
	cp.spawn = savedSpawn;
	syncBuiltinESMExports();
	try {
		if (existsSync(ax)) run("daemon", "stop", "--force");
	} finally {
		await sleep(400);
		rmSync(root, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
	}
}

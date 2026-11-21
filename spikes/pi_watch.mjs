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
		const { AXIOPERA_PI_NOTIFY, AXIOPERA_PI_WATCH } = options.env;
		assert.equal(AXIOPERA_PI_NOTIFY, process.env.AXIOPERA_PI_NOTIFY);
		assert.equal(AXIOPERA_PI_WATCH, AXIOPERA_PI_NOTIFY);
		assert.equal(process.env.AXIOPERA_PI_WATCH, undefined, "a tool shell can register as the watcher");
		const [owner, token] = AXIOPERA_PI_WATCH.split("/");
		assert.equal(owner, args[2]);
		assert.match(token, /^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
		const child = savedSpawn(ax, args, { ...options, env: { ...env, AXIOPERA_PI_NOTIFY, AXIOPERA_PI_WATCH } });
		const record = { child, frames: 0, done: false };
		child.stdout.on("data", () => record.frames++);
		child.once("close", () => { record.done = true; });
		children.push(record);
		return child;
	};
	syncBuiltinESMExports();
	const extension = (await import("../plugins/pi/index.ts")).default;
	const api = {
		on: (name, cb) => { callbacks[name] = cb; },
		sendMessage: (message, opts) => {
			assert.deepEqual(opts, { deliverAs: "steer", triggerTurn: true });
			messages.push({ type: "custom_message", ...message });
		},
	};
	extension(api);

	writeFileSync(info, JSON.stringify({ pid: process.pid }));
	const inherited = "pi:" + other + "/" + randomUUID();
	process.env.AXIOPERA_TASK_ID = "managed-child";
	process.env.AXIOPERA_PI_NOTIFY = inherited;
	start();
	assert.equal(process.env.AXIOPERA_PI_NOTIFY, undefined);
	delete process.env.AXIOPERA_TASK_ID;
	for (const mode of ["rpc", "print", "json"]) {
		process.env.AXIOPERA_PI_NOTIFY = inherited;
		process.env.AXIOPERA_PI_WATCH = inherited;
		start(caller, mode);
		assert.equal(process.env.AXIOPERA_PI_NOTIFY, undefined);
		assert.equal(process.env.AXIOPERA_PI_WATCH, undefined);
	}
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
	assert.equal(process.env.AXIOPERA_PI_NOTIFY, undefined);

	rmSync(info);
	start();
	const firstToken = process.env.AXIOPERA_PI_NOTIFY;
	await failTask(caller, "own");
	await until(() => messages.length === 1, "owner missed its first cold-start task event");
	await failTask(other, "foreign");
	assert.equal(messages.length, 1);
	const first = children.at(-1);
	const seq = messages[0].details.seq;
	first.child.stdin.end();
	await until(() => first.done && children.at(-1) !== first && children.at(-1).frames > 0, "disconnected watch did not replay");
	assert.equal(process.env.AXIOPERA_PI_NOTIFY, firstToken, "reconnect changed the token held by tool shells");
	assert.equal(messages.length, 1);
	// Replay an older entry at the tail of the branch to exercise max-sequence recovery.
	messages.push({ ...messages[0], details: { ...messages[0].details, seq: seq - 1 } });
	start();
	assert.notEqual(process.env.AXIOPERA_PI_NOTIFY, firstToken, "reload did not rotate the subscription");
	await until(() => children.at(-1).frames > 0, "reloaded watch received no replay");
	assert.equal(messages.length, 2);
	start(other);
	assert.ok(process.env.AXIOPERA_PI_NOTIFY.startsWith("pi:" + other + "/"));
	await until(() => messages.length === 3, "session switch missed the new owner's event");
	assert.equal(messages[2].details.caller, "pi:" + other);
	const last = children.at(-1);
	last.child.kill();
	await until(() => last.done, "watch did not exit");
	callbacks.session_shutdown();
	assert.equal(process.env.AXIOPERA_PI_NOTIFY, undefined, "shutdown left a marker for later shell commands");
	const stopped = children.length;
	await sleep(1200);
	assert.equal(children.length, stopped, "shutdown left a retry scheduled");
	start();
	const oldShutdown = callbacks.session_shutdown;
	extension(api);
	start();
	const replacement = process.env.AXIOPERA_PI_NOTIFY;
	oldShutdown();
	assert.equal(process.env.AXIOPERA_PI_NOTIFY, replacement, "old extension revoked the replacement subscription");
	callbacks.session_shutdown();

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
	const previous = messages.at(-1);
	assert.equal(previous.details.home, home);
	messages.push({ ...previous, details: { caller: previous.details.caller, seq: Number.MAX_SAFE_INTEGER } });
	const beforeSwitch = messages.length;
	callbacks.session_shutdown();
	const secondHome = join(root, "second");
	mkdirSync(secondHome);
	copyFileSync(join(home, "config.toml"), join(secondHome, "config.toml"));
	process.env.AXIOPERA_HOME = env.AXIOPERA_HOME = secondHome;
	try {
		start(caller);
		await failTask(caller, "new-home");
		await until(() => messages.length > beforeSwitch, "previous or legacy cursor hid the new home's first event");
		const fresh = messages.at(-1).details;
		assert.equal(fresh.home, secondHome);
		assert.equal(fresh.caller, previous.details.caller);
		assert.ok(fresh.seq < previous.details.seq, "new daemon did not exercise a lower sequence");
	} finally {
		callbacks.session_shutdown();
		try {
			run("daemon", "stop", "--force");
		} finally {
			process.env.AXIOPERA_HOME = env.AXIOPERA_HOME = home;
		}
	}
	console.log("Pi watcher passed startup retry, replay, reload, session and home isolation, parent death, idle exit and restart delivery");
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

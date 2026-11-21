import { spawn } from "node:child_process";
import { readFileSync, unwatchFile, watchFile } from "node:fs";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import { createInterface } from "node:readline";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const notice = "axiopera-inbox";

export default function (pi: ExtensionAPI) {
	let stop: (() => void) | undefined;

	pi.on("session_start", (_event, ctx) => {
		stop?.();
		if (ctx.mode !== "tui" || process.env.AXIOPERA_TASK_ID) return;

		const caller = `pi:${ctx.sessionManager.getSessionId()}`;
		const home = resolve(ctx.cwd, process.env.AXIOPERA_HOME || join(homedir(), ".axiopera"));
		const info = join(home, "daemon", "info.json");
		let lastSeq = 0;
		for (const entry of ctx.sessionManager.getBranch()) {
			if (entry.type === "custom_message" && entry.customType === notice) {
				const details = entry.details as { caller?: string; seq?: number } | undefined;
				if (details?.caller === caller && typeof details.seq === "number" && Number.isSafeInteger(details.seq)) {
					lastSeq = Math.max(lastSeq, details.seq);
				}
			}
		}

		let closed = false;
		let watching: ReturnType<typeof spawn> | undefined;
		let timer: ReturnType<typeof setTimeout> | undefined;
		let delay = 500;
		function retry() {
			if (closed || timer) return;
			timer = setTimeout(() => { timer = undefined; connect(); }, delay);
			timer.unref();
			delay = Math.min(delay * 2, 30_000);
		}
		function connect() {
			if (closed || watching) return;
			// A stopped daemon leaves info.json behind. Only probe a live PID.
			try {
				const { pid } = JSON.parse(readFileSync(info, "utf8"));
				if (!Number.isSafeInteger(pid) || pid <= 0) return retry();
				process.kill(pid, 0);
			} catch (error) {
				const code = (error as NodeJS.ErrnoException).code;
				if (code === "ENOENT" || code === "ESRCH") return;
				if (code !== "EPERM") return retry();
			}
			const child = spawn("axiopera", ["--json", "--caller", caller, "watch", "--if-running", "--until-stdin-closes"], {
				cwd: ctx.cwd,
				stdio: ["pipe", "pipe", "ignore"],
				windowsHide: true,
			});
			watching = child;
			const lines = createInterface({ input: child.stdout });
			lines.on("line", (line) => {
				if (closed) return;
				let data;
				try {
					data = JSON.parse(line).data;
				} catch {
					return;
				}
				if (!Array.isArray(data)) return;
				delay = 500;
				let seq = lastSeq;
				for (const event of data) {
					if (Number.isSafeInteger(event?.seq)) seq = Math.max(seq, event.seq);
				}
				if (seq === lastSeq) return;
				lastSeq = seq;
				pi.sendMessage(
					{ customType: notice, content: "Run axiopera inbox to read your task updates.", display: true, details: { caller, seq } },
					{ deliverAs: "steer", triggerTurn: true },
				);
			});
			child.on("error", () => { /* Axiopera may not be installed on this host. */ });
			child.once("close", () => {
				lines.close();
				watching = undefined;
				retry();
			});
		}

		// A daemon writes this file on startup. An unused Pi session never starts a daemon.
		function changed() {
			clearTimeout(timer);
			timer = undefined;
			delay = 500;
			connect();
		}
		watchFile(info, { persistent: false, interval: 2000 }, changed);
		stop = () => {
			closed = true;
			clearTimeout(timer);
			unwatchFile(info, changed);
			watching?.kill();
			stop = undefined;
		};
		connect();
	});

	pi.on("session_shutdown", () => stop?.());
}

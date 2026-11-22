// RPC exposes a catalog snapshot. Wait for live provider extensions before selecting their models.
import { SettingsManager, getAgentDir, resolveCliModel } from "@earendil-works/pi-coding-agent";
import { clampThinkingLevel } from "@earendil-works/pi-ai";

export default function (pi) {
  pi.registerFlag("axiopera-catalog", { type: "string", description: "Axiopera RPC model selection" });
  pi.on("session_start", async (_, ctx) => {
    const options = JSON.parse(pi.getFlag("axiopera-catalog"));
    try {
      let loaded;
      const settings = () => loaded ??= SettingsManager.create(ctx.cwd, getAgentDir(), { projectTrusted: ctx.isProjectTrusted() });
      await select(pi, ctx, options, settings);
      if (options.configured) {
        const model = ctx.modelRegistry.getAvailable().find(m => m.provider + "/" + m.id === options.configured);
        if (model) ctx.ui.setStatus("axiopera-configured-effort", clampThinkingLevel(model, preference(settings(), model)));
      }
      ctx.ui.setStatus("axiopera-catalog", "ready");
    } catch (error) {
      ctx.ui.setStatus("axiopera-catalog", String(error.message));
    }
  });
}

function preference(settings, model) {
  return settings.getModelThinkingLevel(model.provider, model.id) ?? settings.getDefaultThinkingLevel() ?? "medium";
}

async function select(pi, ctx, options, getSettings) {
  const registry = ctx.modelRegistry;
  const dynamic = registry.getRegisteredProviderIds().filter(id =>
    registry.getRegisteredProviderConfig(id)?.refreshModels || registry.getRegisteredNativeProvider(id)?.refreshModels
  );
  let provider = ctx.model?.provider;
  let id = ctx.model?.id;
  let saved;
  let settings;
  if (dynamic.length) {
    saved = options.resume ? ctx.sessionManager.buildSessionProjection() : undefined;
    settings = getSettings();
    if (options.model) {
      // Pi may reject a provider with no static models before it enters RPC.
      if (options.retry) {
        provider = dynamic.filter(p => options.model.startsWith(p + "/")).sort((a, b) => b.length - a.length)[0];
        id = provider && options.model.slice(provider.length + 1);
      }
    } else if (saved?.model && dynamic.includes(saved.model.provider)) {
      provider = saved.model.provider;
      id = saved.model.modelId;
    } else if (!options.resume && !ctx.scopedModels.length && dynamic.includes(settings.getDefaultProvider())) {
      provider = settings.getDefaultProvider();
      id = settings.getDefaultModel();
    }
  }
  const missing = !ctx.model || ctx.model.api === "unknown" && ctx.model.provider === "unknown" && ctx.model.id === "unknown";
  const requested = dynamic.includes(provider);
  const providers = options.catalog || missing ? dynamic : requested ? [provider] : [];
  if (providers.length) {
    // A failed provider must not hide the rest of the directory or block a working cached model.
    const signal = AbortSignal.timeout(12_000);
    const result = await Promise.race([
      registry.refresh({ providers, signal }),
      new Promise(resolve => signal.addEventListener("abort", () => resolve({ aborted: true }), { once: true })),
    ]);
    const available = registry.getAvailable();
    let selectedEffort;
    if (requested && options.model) {
      // Re-run Pi's own CLI matching against the completed catalog (including fuzzy IDs and :level).
      const selected = resolveCliModel({
        cliModel: options.model, cliThinking: options.effort,
        modelRuntime: { getModels: () => available, hasConfiguredAuth: p => available.some(m => m.provider === p) },
      });
      if (selected.model && !selected.warning) {
        provider = selected.model.provider;
        id = selected.model.id;
        selectedEffort = selected.thinkingLevel;
      }
    }
    let model = available.find(m => m.provider === provider && m.id === id);
    if (!model && missing && !requested && !options.model) model = available[0];
    if (model && (requested || missing)) {
      const level = options.effort ?? selectedEffort ?? saved?.thinkingLevel ?? preference(settings, model);
      if (!await pi.setModel(model)) throw new Error("No API key for " + model.provider + "/" + model.id);
      pi.setThinkingLevel(level);
      return;
    }
    if (requested && !options.catalog) {
      if (result.aborted) throw new Error("Pi model discovery timed out for " + provider);
      if (result.errors.has(provider)) throw new Error("Pi model discovery failed for " + provider);
      throw new Error("Pi model not found: " + provider + "/" + id);
    }
  }
  if (options.retry) throw new Error("Pi model not found: " + options.model);
}

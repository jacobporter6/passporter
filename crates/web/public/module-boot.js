// Loads an on-demand WASM module for src/lazy.rs. A classic script (the
// CSP forbids inline ones) that the page inserts once per attempt, with
// data-module naming the module (/<name>.js and /<name>_bg.wasm, built from
// src/bin/<name>.rs) and data-attempt counting attempts. A failed import
// stays failed for its URL until the page reloads, so a retry imports a new
// one; the service worker serves shell files by path, so that still works
// offline. The module's promise is left in passporterModules[name].
// A block, so its bindings don't clash with the next run's: every run of a
// classic script shares the page's global lexical scope.
{
  const { module: name, attempt } = document.currentScript.dataset;
  const retry = attempt === "1" ? "" : `?attempt=${attempt}`;
  (globalThis.passporterModules ??= {})[name] = import(`/${name}.js${retry}`).then(
    async (module) => {
      await module.default({ module_or_path: `/${name}_bg.wasm` });
      return module;
    },
  );
}

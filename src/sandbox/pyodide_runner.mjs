// SPDX-License-Identifier: AGPL-3.0-or-later
//
// Node-hosted Pyodide runner for MIRA's scientific code-execution backend.
// Embedded in the binary (include_str!) and written to the Pyodide deps dir at
// provision time; invoked by `src/sandbox/pyodide.rs` as:
//
//     node <deps>/pyodide_runner.mjs <request.json>
//
// with cwd = the deps dir (so `import "pyodide"` resolves node_modules/pyodide).
//
// request.json: {
//   code: string,                 // Python source to run
//   packages?: string[],          // explicit packages to loadPackage
//   load_from_imports?: bool,     // also auto-load packages from the code's imports
//   stdin?: string,               // fed to sys.stdin
//   output_dir?: string,          // host dir mounted at /tmp/output (matplotlib savefig, etc.)
//   cache_dir?: string,           // on-disk wheel cache (packageCacheDir) — offline reuse + pre-warm
//   mem_mb?: number               // advisory; enforced by the parent via node flags
// }
//
// Emits exactly one line to stdout prefixed with the marker, holding the JSON
// result the Rust side parses:
//   __MIRA_PYODIDE_RESULT__{"ok":bool,"stdout":string,"stderr":string,"error":string|null}
//
// Isolation: the Python runs in wasm (V8) — no syscalls, no host FS except the
// single mounted /output, no network from user code. The Node host process is
// privileged (see design-docs/code-execution-sandbox.md security note).

import { loadPyodide } from "pyodide";
import { readFileSync } from "node:fs";

const MARKER = "__MIRA_PYODIDE_RESULT__";

async function main() {
  const reqPath = process.argv[2];
  if (!reqPath) throw new Error("usage: pyodide_runner.mjs <request.json>");
  const req = JSON.parse(readFileSync(reqPath, "utf8"));

  let stdout = "";
  let stderr = "";
  const out = { ok: false, stdout: "", stderr: "", error: null };

  try {
    const opts = {
      stdout: (s) => { stdout += s + "\n"; },
      stderr: (s) => { stderr += s + "\n"; },
    };
    // Cache downloaded wheels to disk so the first scientific run pays the
    // network cost once; later runs (and offline use) load from here. Set by
    // the Rust side to <deps>/cache; also written during pre-warm.
    if (req.cache_dir) opts.packageCacheDir = req.cache_dir;
    const py = await loadPyodide(opts);

    // Mount the host output dir → /tmp/output so files the script writes (e.g.
    // matplotlib savefig) land on the host for artifact capture. The mount
    // point matches SANDBOX_OUTPUT_DIR in src/tools/code_run.rs so the model's
    // "save to /tmp/output/" instruction works identically across backends.
    if (req.output_dir) {
      py.FS.mkdirTree("/tmp/output");
      py.FS.mount(py.FS.filesystems.NODEFS, { root: req.output_dir }, "/tmp/output");
    }

    if (req.stdin) {
      // Feed stdin: override sys.stdin with a StringIO.
      py.setStdin({ stdin: () => req.stdin });
    }

    if (req.load_from_imports) {
      await py.loadPackagesFromImports(req.code);
    }
    if (Array.isArray(req.packages) && req.packages.length) {
      await py.loadPackage(req.packages);
    }

    await py.runPythonAsync(req.code);
    out.ok = true;
  } catch (e) {
    out.error = String(e && e.message ? e.message : e);
  }

  out.stdout = stdout;
  out.stderr = stderr;
  process.stdout.write(MARKER + JSON.stringify(out));
}

main().catch((e) => {
  process.stdout.write(MARKER + JSON.stringify({ ok: false, stdout: "", stderr: "", error: String(e) }));
  process.exit(1);
});

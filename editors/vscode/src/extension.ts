// The VS Code client for `magequery lsp`. Thin by design: find (or fetch) the server
// binary, hand everything else to vscode-languageclient. The server owns all language
// smarts; the one client-side feature is mapping the server's
// `magequery.showReferences` code-lens command onto VS Code's peek view.

import * as fs from "node:fs";
import * as path from "node:path";
import { execFile, spawn } from "node:child_process";
import { promisify } from "node:util";
import * as vscode from "vscode";
import {
  LanguageClient,
  LanguageClientOptions,
  ServerOptions,
} from "vscode-languageclient/node";

const execFileAsync = promisify(execFile);

/** Oldest server this client speaks to (the `lsp` subcommand's introduction). */
const MIN_SERVER_VERSION = "0.5.0";
const REPO = "cresset-tools/magequery";

let client: LanguageClient | undefined;

/** The binary the running client was started with, set by bootstrap (undefined = not running). */
let serverBinary: string | undefined;

export function activate(context: vscode.ExtensionContext): void {
  // Code lenses arrive with this command; convert the plain-JSON arguments into the
  // types VS Code's built-in peek view expects.
  context.subscriptions.push(
    vscode.commands.registerCommand(
      "magequery.showReferences",
      async (uri: string, position: { line: number; character: number }, locations: unknown[]) => {
        if (!client) {
          return;
        }
        await vscode.commands.executeCommand(
          "editor.action.showReferences",
          vscode.Uri.parse(uri),
          new vscode.Position(position.line, position.character),
          locations.map((location) => client!.protocol2CodeConverter.asLocation(location as never)),
        );
      },
    ),
  );

  // A manual "is there a newer server?" trigger; unlike the startup check it ignores the
  // per-version dismissal and reports even when already current.
  context.subscriptions.push(
    vscode.commands.registerCommand("magequery.checkForServerUpdate", () =>
      checkForUpdate(context, { force: true }),
    ),
  );

  // Never block activation on the bootstrap: findServer may ask the user a question
  // (the download prompt), and an unanswered notification would otherwise pin the
  // extension in "Activating…" forever. Once the server is up, quietly check for a newer
  // release (only the binary we downloaded ourselves is ours to update).
  void bootstrap(context)
    .then(() => {
      if (serverBinary) {
        void checkForUpdate(context, { force: false });
      }
    })
    .catch((error) => {
      void vscode.window.showErrorMessage(`magequery failed to start: ${String(error)}`);
    });
}

async function bootstrap(context: vscode.ExtensionContext): Promise<void> {
  const binary = await findServer(context);
  if (!binary) {
    return; // findServer already surfaced the reason
  }
  serverBinary = binary;

  const serverOptions: ServerOptions = {
    command: binary,
    args: ["lsp"],
  };
  const clientOptions: LanguageClientOptions = {
    documentSelector: [
      { scheme: "file", language: "php" },
      { scheme: "file", language: "xml" },
      { scheme: "file", pattern: "**/*.graphqls" },
      { scheme: "file", pattern: "**/*.phtml" },
    ],
  };
  client = new LanguageClient("magequery", "magequery", serverOptions, clientOptions);
  await client.start();
}

export async function deactivate(): Promise<void> {
  await client?.stop();
}

// ---- server binary resolution --------------------------------------------------------
// Priority: explicit setting → PATH → previously downloaded → download from the GitHub
// release (cargo-dist artifact naming). A PATH binary older than MIN_SERVER_VERSION is
// treated as absent so the download path can supply a current one.

async function findServer(context: vscode.ExtensionContext): Promise<string | undefined> {
  const configured = vscode.workspace.getConfiguration("magequery").get<string>("serverPath");
  if (configured) {
    if (await versionOf(configured)) {
      return configured;
    }
    void vscode.window.showErrorMessage(
      `magequery.serverPath (${configured}) is not a runnable magequery binary.`,
    );
    return undefined;
  }

  const onPath = process.platform === "win32" ? "magequery.exe" : "magequery";
  const pathVersion = await versionOf(onPath);
  if (pathVersion && !olderThan(pathVersion, MIN_SERVER_VERSION)) {
    return onPath;
  }

  const downloaded = downloadTarget(context);
  const downloadedVersion = downloaded && (await versionOf(downloaded));
  if (downloadedVersion && !olderThan(downloadedVersion, MIN_SERVER_VERSION)) {
    return downloaded;
  }

  const reason = pathVersion
    ? `magequery ${pathVersion} on PATH is older than ${MIN_SERVER_VERSION}`
    : "magequery was not found on PATH";
  const pick = await vscode.window.showInformationMessage(
    `${reason}. Download the current release from GitHub?`,
    "Download",
    "Cancel",
  );
  if (pick !== "Download") {
    return undefined;
  }
  try {
    return await download(context, await latestReleaseTag());
  } catch (error) {
    void vscode.window.showErrorMessage(`magequery download failed: ${String(error)}`);
    return undefined;
  }
}

// ---- update check --------------------------------------------------------------------
// Only the binary this extension downloaded is ours to update: a PATH or `serverPath`
// binary is the user's (or their package manager's) to bump, so the startup check stays
// silent about it and the manual command just explains that.

async function checkForUpdate(
  context: vscode.ExtensionContext,
  { force }: { force: boolean },
): Promise<void> {
  const enabled = vscode.workspace
    .getConfiguration("magequery")
    .get<boolean>("checkForUpdates", true);
  if (!force && !enabled) {
    return;
  }

  const managed = downloadTarget(context);
  if (!serverBinary || !managed || serverBinary !== managed) {
    if (force) {
      void vscode.window.showInformationMessage(
        "magequery is supplied from PATH or magequery.serverPath — the extension only updates a binary it downloaded itself.",
      );
    }
    return;
  }

  const current = await versionOf(serverBinary);
  if (!current) {
    return;
  }

  let tag: string;
  try {
    tag = await latestReleaseTag();
  } catch (error) {
    if (force) {
      void vscode.window.showErrorMessage(`magequery update check failed: ${String(error)}`);
    }
    return;
  }
  const latest = tag.replace(/^magequery-v/, "");

  if (!olderThan(current, latest)) {
    if (force) {
      void vscode.window.showInformationMessage(`magequery ${current} is up to date.`);
    }
    return;
  }

  // "Later" suppresses re-prompting for this version until a newer one appears; the manual
  // command bypasses the suppression.
  if (!force && context.globalState.get<string>("magequery.dismissedVersion") === latest) {
    return;
  }

  const pick = await vscode.window.showInformationMessage(
    `magequery ${latest} is available (you have ${current}).`,
    "Update",
    "Later",
  );
  if (pick === "Update") {
    try {
      await updateServer(context, tag);
    } catch (error) {
      void vscode.window.showErrorMessage(`magequery update failed: ${String(error)}`);
    }
  } else if (pick === "Later") {
    await context.globalState.update("magequery.dismissedVersion", latest);
  }
}

async function updateServer(context: vscode.ExtensionContext, tag: string): Promise<void> {
  // Stop the running server first so its binary file is free to overwrite (Windows locks a
  // running exe; Linux refuses to write a busy one), then swap in place and restart.
  await client?.stop();
  client = undefined;
  serverBinary = undefined;
  await download(context, tag);
  await bootstrap(context);
}

async function latestReleaseTag(): Promise<string> {
  const release = (await (
    await fetch(`https://api.github.com/repos/${REPO}/releases/latest`)
  ).json()) as { tag_name?: string };
  if (!release.tag_name) {
    throw new Error("latest release has no tag_name");
  }
  return release.tag_name;
}

async function versionOf(binary: string): Promise<string | undefined> {
  try {
    const { stdout } = await execFileAsync(binary, ["--version"]);
    return stdout.trim().split(/\s+/).pop();
  } catch {
    return undefined;
  }
}

function olderThan(version: string, minimum: string): boolean {
  const parse = (v: string) => v.split(".").map((part) => Number.parseInt(part, 10) || 0);
  const [a, b] = [parse(version), parse(minimum)];
  for (let i = 0; i < 3; i++) {
    if ((a[i] ?? 0) !== (b[i] ?? 0)) {
      return (a[i] ?? 0) < (b[i] ?? 0);
    }
  }
  return false;
}

/** cargo-dist target triple for this machine, or undefined when we ship no binary. */
function distTriple(): string | undefined {
  const key = `${process.platform}-${process.arch}`;
  return {
    "linux-x64": "x86_64-unknown-linux-gnu",
    "linux-arm64": "aarch64-unknown-linux-gnu",
    "darwin-arm64": "aarch64-apple-darwin",
    "darwin-x64": "x86_64-apple-darwin",
    "win32-x64": "x86_64-pc-windows-msvc",
  }[key];
}

function downloadTarget(context: vscode.ExtensionContext): string | undefined {
  const triple = distTriple();
  if (!triple) {
    return undefined;
  }
  const name = process.platform === "win32" ? "magequery.exe" : "magequery";
  return path.join(context.globalStorageUri.fsPath, "server", name);
}

async function download(context: vscode.ExtensionContext, tag: string): Promise<string> {
  const triple = distTriple();
  const target = downloadTarget(context);
  if (!triple || !target) {
    throw new Error(`no prebuilt binary for ${process.platform}-${process.arch}`);
  }
  const archiveExt = process.platform === "win32" ? "zip" : "tar.gz";
  const url = `https://github.com/${REPO}/releases/download/${tag}/magequery-${triple}.${archiveExt}`;

  const dir = path.dirname(target);
  await fs.promises.mkdir(dir, { recursive: true });
  const archive = path.join(dir, `archive.${archiveExt}`);
  const body = await fetch(url);
  if (!body.ok) {
    throw new Error(`${url}: HTTP ${body.status}`);
  }
  await fs.promises.writeFile(archive, Buffer.from(await body.arrayBuffer()));

  // bsdtar ships with macOS, Linux distros, and Windows 10+, and reads both formats.
  await new Promise<void>((resolve, reject) => {
    const tar = spawn("tar", ["-xf", archive, "-C", dir]);
    tar.on("error", reject);
    tar.on("exit", (code) =>
      code === 0 ? resolve() : reject(new Error(`tar exited with ${code}`)),
    );
  });
  await fs.promises.rm(archive, { force: true });
  // cargo-dist archives contain the bare binary (auto-includes = false).
  if (process.platform !== "win32") {
    await fs.promises.chmod(target, 0o755);
  }
  void vscode.window.showInformationMessage(
    `magequery ${tag.replace(/^magequery-v/, "")} downloaded.`,
  );
  return target;
}

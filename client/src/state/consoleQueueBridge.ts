// The seam between the console queue (uploadQueue) and the install code
// (pkgLibrary). uploadQueue already imports pkgLibrary, so pkgLibrary must not
// import uploadQueue: each side registers its half here instead. Types only —
// this module imports no store.
import type { ExternalPkg } from "../api/ps5";
import type { LinkInstallMode } from "./linkInstallPrefs";

/** What to install and where it comes from. */
export type InstallRequest =
  /** A package already staged in the library on the PS5. */
  | { via: "library"; path: string }
  /** Any package path on the PS5 (File System). */
  | { via: "console-path"; path: string }
  /** A package found on an external drive (External Packages). */
  | { via: "external"; pkg: ExternalPkg }
  /** A file on this computer or on a saved server, streamed to the PS5. */
  | { via: "stream"; source: string }
  /** A download link. Never persisted: it can carry a signed token. */
  | { via: "link"; url: string; mode: LinkInstallMode; insecureTls: boolean };

export interface InstallResult {
  ok: boolean;
  message?: string;
  mayNotLaunch?: boolean;
  /** A stream install the PS5 never fetched from: uploading it may work. */
  stagedFallbackRecommended?: boolean;
}

export interface InstallHooks {
  onProgress: (pct: number) => void;
  onStatus: (msg: string) => void;
}

export type InstallExecutor = (
  req: InstallRequest,
  host: string,
  hooks: InstallHooks,
) => Promise<InstallResult>;

export interface EnqueueInstallInput {
  host: string;
  request: InstallRequest;
  displayName: string;
  contentId?: string | null;
  /** PARAM.SFO category (gd / gp / ac): orders base → update → DLC. */
  category?: string | null;
}

export interface EnqueuedInstall {
  id: string;
  /** Resolves when the item finishes, or is removed from the queue. */
  done: Promise<InstallResult>;
}

let executor: InstallExecutor | null = null;
let enqueuer: ((input: EnqueueInstallInput) => EnqueuedInstall) | null = null;

export function registerInstallExecutor(fn: InstallExecutor): void {
  executor = fn;
}

export function getInstallExecutor(): InstallExecutor | null {
  return executor;
}

export function registerInstallEnqueuer(
  fn: (input: EnqueueInstallInput) => EnqueuedInstall,
): void {
  enqueuer = fn;
}

export function enqueueInstall(input: EnqueueInstallInput): EnqueuedInstall {
  if (!enqueuer) throw new Error("The install queue is not ready yet.");
  return enqueuer(input);
}

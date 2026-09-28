// One picked Upload source → the console-queue item that uploads it. Shared by a single pick
// and a batch, so both queue exactly the same thing.

import type { PickedSource } from "../state/upload";
import type { AddQueueItem } from "../state/uploadQueue";
import { stagingBasename, stagingSubdirForCategory } from "./pkgStagingPath";
import { resolveUploadDest } from "./uploadDest";

export interface UploadItemOptions {
  addr: string;
  destinationVolume: string | null;
  destinationSubpath: string;
  archiveIntoSubfolder: boolean;
  reconcileMode: AddQueueItem["reconcileMode"];
  strategy: "overwrite" | "resume";
  excludes: string[];
  mountAfterUpload: boolean;
  mountReadOnly: boolean;
  registerAfterUpload: boolean;
  installAfterUpload: boolean;
  deletePkgAfterInstall: boolean;
  /** Where packages stage: the console's package drive (see lib/pkgStorage). */
  pkgDir: string;
  /** Uniqueness for a package's staged name. */
  nonce: string;
  now: number;
}

function baseName(path: string): string {
  return path.replace(/[\\/]+$/, "").split(/[\\/]/).pop() ?? path;
}

export function buildUploadQueueItem(
  source: PickedSource,
  rarPassword: string | null,
  o: UploadItemOptions,
): AddQueueItem {
  // A package stages into the package library and the queue's finisher installs it.
  if (source.kind === "pkg") {
    const info = source.pkgInfo ?? null;
    const cid = info?.contentId ?? "";
    const name = stagingBasename(cid, o.nonce, o.now);
    const subdir = stagingSubdirForCategory(info?.category ?? null);
    return {
      sourceKind: "pkg",
      sourcePath: source.path,
      displayName: info?.title?.trim() || baseName(source.path),
      resolvedDest: subdir ? `${o.pkgDir}/${subdir}/${name}` : `${o.pkgDir}/${name}`,
      addr: o.addr,
      strategy: "overwrite",
      reconcileMode: o.reconcileMode,
      excludes: [],
      contentId: cid,
      category: info?.category ?? null,
      installAfterUpload: o.installAfterUpload,
      deletePkgAfterInstall: o.deletePkgAfterInstall,
      mountAfterUpload: false,
      mountReadOnly: o.mountReadOnly,
      registerAfterUpload: false,
    };
  }
  const { dest } = resolveUploadDest(
    o.destinationVolume,
    o.destinationSubpath,
    source.path,
    source.kind === "archive",
    o.archiveIntoSubfolder,
  );
  return {
    sourceKind: source.kind,
    sourcePath: source.path,
    displayName: baseName(source.path),
    resolvedDest: dest,
    addr: o.addr,
    strategy: o.strategy,
    reconcileMode: o.reconcileMode,
    excludes: o.excludes,
    rarPassword: source.kind === "archive" ? rarPassword : null,
    mountAfterUpload: source.kind === "image" && o.mountAfterUpload,
    mountReadOnly: o.mountReadOnly,
    registerAfterUpload: source.kind === "game-folder" && o.registerAfterUpload,
  };
}

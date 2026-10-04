// "From another PS5": which consoles can be the source, and whether a request is complete.

import { hostOf } from "./addr";

export interface RosterConsole {
  id: string;
  name: string;
  host: string;
}

/** The consoles a copy to `destHost` can come from: every other console in the roster
 *  (a console cannot be its own source, and the engine refuses it). */
export function sourceCandidates<T extends RosterConsole>(
  profiles: readonly T[],
  destHost: string,
): T[] {
  const dest = hostOf(destHost);
  return profiles.filter((p) => hostOf(p.host) !== "" && hostOf(p.host) !== dest);
}

/** Why a PS5 to PS5 request cannot start yet, or null when it can. */
export type Ps5SourceProblem =
  | "no_destination_console"
  | "no_source_console"
  | "same_console"
  | "no_source_path"
  | "no_destination_path";

export function ps5SourceProblem(req: {
  fromHost: string;
  srcPath: string;
  toHost: string;
  destPath: string;
}): Ps5SourceProblem | null {
  if (!hostOf(req.toHost)) return "no_destination_console";
  if (!hostOf(req.fromHost)) return "no_source_console";
  if (hostOf(req.fromHost) === hostOf(req.toHost)) return "same_console";
  if (!req.srcPath.trim().startsWith("/")) return "no_source_path";
  if (!req.destPath.trim().startsWith("/")) return "no_destination_path";
  return null;
}

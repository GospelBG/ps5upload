import { useState } from "react";
import { ChevronDown, Search } from "lucide-react";

import { GameIcon } from "../../components";
import { hasVersionMatch, type CheatGame, type CheatSection } from "../../lib/cheatGames";
import { hostOf } from "../../lib/addr";
import { useTr } from "../../state/lang";
import type { AppInfoDetails } from "../../api/ps5";

/** The left column: every game on the console, grouped by what you can do
 *  with it next — play with cheats, switch cheats on, download some. */
export function CheatGameList({
  host,
  sections,
  query,
  onQuery,
  selectedId,
  onSelect,
  details,
}: {
  host: string;
  sections: CheatSection[];
  query: string;
  onQuery: (q: string) => void;
  selectedId: string | null;
  onSelect: (titleId: string) => void;
  details: Map<string, AppInfoDetails | null>;
}) {
  const tr = useTr();
  // Games nobody has published cheats for are listed, but folded: they are
  // usually apps (YouTube, a browser, a loader), and a player scanning for
  // something to cheat in shouldn't have to read past them.
  const [showNone, setShowNone] = useState(false);
  const heading: Record<CheatSection["key"], string> = {
    playing: tr("cheats_section_playing", undefined, "Playing now"),
    ready: tr("cheats_section_ready", undefined, "Cheats ready"),
    available: tr("cheats_section_available", undefined, "Cheats to download"),
    none: tr("cheats_section_none", undefined, "No cheats published"),
  };
  const total = sections.reduce((n, s) => n + s.games.length, 0);

  return (
    <div className="flex min-w-0 flex-col gap-3">
      <label className="relative block">
        <Search
          size={14}
          className="pointer-events-none absolute left-2.5 top-1/2 -translate-y-1/2 text-[var(--color-muted)]"
        />
        <input
          type="search"
          value={query}
          onChange={(e) => onQuery(e.target.value)}
          placeholder={tr("cheats_find_game", undefined, "Find a game")}
          aria-label={tr("cheats_find_game", undefined, "Find a game")}
          className="input"
          // `.input` is plain CSS and sets its own padding, which a utility
          // class cannot beat — without this the icon sits on the text.
          style={{ paddingLeft: "2rem" }}
        />
      </label>

      {total === 0 && (
        <p className="px-1 py-4 text-center text-sm text-[var(--color-muted)]">
          {query
            ? tr("cheats_no_game_match", undefined, "No game matches that search.")
            : tr("cheats_no_games", undefined, "No games found on this PS5.")}
        </p>
      )}

      {sections.map((section) => {
        const folded = section.key === "none" && !showNone && !query;
        return (
          <section key={section.key}>
            {section.key === "none" ? (
              <button
                type="button"
                onClick={() => setShowNone((v) => !v)}
                className="mb-1.5 flex w-full items-center gap-2 text-[11px] font-semibold uppercase tracking-wide text-[var(--color-muted)] hover:text-[var(--color-text)]"
                aria-expanded={!folded}
              >
                <span>{heading[section.key]}</span>
                <span className="rounded-full bg-[var(--color-surface-3)] px-1.5 font-mono text-[10px] tabular-nums">
                  {section.games.length}
                </span>
                <ChevronDown
                  size={12}
                  className={`ml-auto transition-transform ${folded ? "" : "rotate-180"}`}
                />
              </button>
            ) : (
              <div className="mb-1.5 flex items-center gap-2 text-[11px] font-semibold uppercase tracking-wide text-[var(--color-muted)]">
                <span>{heading[section.key]}</span>
                <span className="rounded-full bg-[var(--color-surface-3)] px-1.5 font-mono text-[10px] tabular-nums">
                  {section.games.length}
                </span>
              </div>
            )}
            {!folded && (
              <ul className="grid gap-1">
                {section.games.map((g) => (
                  <GameRow
                    key={g.titleId}
                    host={host}
                    game={g}
                    selected={g.titleId === selectedId}
                    version={details.get(g.titleId)?.version ?? null}
                    onSelect={() => onSelect(g.titleId)}
                  />
                ))}
              </ul>
            )}
          </section>
        );
      })}
    </div>
  );
}

function GameRow({
  host,
  game,
  selected,
  version,
  onSelect,
}: {
  host: string;
  game: CheatGame;
  selected: boolean;
  version: string | null;
  onSelect: () => void;
}) {
  const tr = useTr();
  const n = game.available.length;
  const match = version ? hasVersionMatch(game.available, version) : false;
  return (
    <li>
      <button
        type="button"
        onClick={onSelect}
        aria-current={selected ? "true" : undefined}
        className={`flex w-full items-center gap-3 rounded-md border px-2.5 py-2 text-left transition-colors ${
          selected
            ? "border-[var(--color-accent)] bg-[var(--color-accent-soft)]"
            : "border-transparent hover:border-[var(--color-border)] hover:bg-[var(--color-surface-2)]"
        }`}
      >
        <GameIcon host={hostOf(host)} titleId={game.titleId} size={40} />
        <span className="min-w-0 flex-1">
          <span className="line-clamp-2 text-sm font-medium [overflow-wrap:anywhere]">
            {game.name}
          </span>
          <span className="mt-0.5 block truncate font-mono text-[11px] text-[var(--color-muted)]">
            {game.titleId}
            {version ? ` · v${version}` : ""}
          </span>
        </span>
        <span className="shrink-0 text-right text-[11px] font-medium">
          {game.running ? (
            <span className="inline-flex items-center gap-1 text-[var(--color-good)]">
              <span className="h-1.5 w-1.5 rounded-full bg-[var(--color-good)]" />
              {tr("cheats_badge_playing", undefined, "Playing")}
            </span>
          ) : game.downloaded ? (
            <span className="text-[var(--color-accent)]">
              {tr("cheats_badge_ready", undefined, "Ready")}
            </span>
          ) : n > 0 ? (
            <span className={match ? "text-[var(--color-good)]" : "text-[var(--color-muted)]"}>
              {tr("cheats_badge_count", { n }, `${n} available`)}
              {match && (
                <span className="block text-[10px] font-normal">
                  {tr("cheats_badge_your_version", undefined, "for your version")}
                </span>
              )}
            </span>
          ) : null}
        </span>
      </button>
    </li>
  );
}

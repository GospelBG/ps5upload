// The package viewer: what a package, image or game folder is — cover, badges,
// whether the selected console can run it — with Overview and Details tabs.
// `PackagePanel` loads; `PackagePanelView` only renders (and is what the tests
// render). Actions come from the screen that opened it: the panel never
// installs or uploads by itself.

import { useEffect, useMemo, useRef, useState } from "react";
import { AlertTriangle, CheckCircle2, CircleHelp, Copy, XCircle } from "lucide-react";

import { Drawer } from "./Drawer";
import { Button, ErrorCard, Spinner } from "./index";
import { gameInspect, gameInspectImageUrl, type GameInspection } from "../api/gameInspect";
import { installFreeBytes, pkgInstallPreflight } from "../api/ps5";
import { formatBytes } from "../lib/format";
import { hostOf, transferAddr } from "../lib/addr";
import { parsePS5Firmware } from "../lib/ps5Firmware";
import { useTitleInfo } from "../lib/useTitleInfo";
import {
  firmwareCheck,
  installedCheck,
  spaceCheck,
  type Verdict,
} from "../lib/packageChecks";
import { useConnectionStore } from "../state/connection";
import { useTr } from "../state/lang";

export interface PanelAction {
  label: string;
  onClick: () => void;
  primary?: boolean;
}

export interface PanelChecks {
  firmware: ReturnType<typeof firmwareCheck>;
  installed: (ReturnType<typeof installedCheck> & { installedVer: string | null }) | null;
  space: ReturnType<typeof spaceCheck> | null;
}

type Tab = "overview" | "details";

/** The PARAM keys worth seeing first; the rest sit behind Show all. */
const CURATED_KEYS = new Set([
  "TITLE",
  "TITLE_ID",
  "CONTENT_ID",
  "APP_VER",
  "VERSION",
  "CATEGORY",
  "SYSTEM_VER",
  "PUBTOOLINFO",
  "titleId",
  "contentId",
  "contentVersion",
  "masterVersion",
  "requiredSystemSoftwareVersion",
  "sdkVersion",
  "applicationDrmType",
]);

/** `changeinfo.xml` as readable text: tags out, entities decoded. */
export function changeNotesText(xml: string): string {
  return xml
    // Notes are usually wrapped in CDATA: keep the text, drop the wrapper.
    .replace(/<!\[CDATA\[([\s\S]*?)\]\]>/g, (_m, text: string) => `\n${text}\n`)
    .replace(/<\?[^>]*\?>/g, "\n")
    .replace(/<[^>]*>/g, "\n")
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&quot;/g, '"')
    .replace(/&apos;/g, "'")
    .replace(/&amp;/g, "&")
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean)
    .join("\n");
}

function VerdictIcon({ v }: { v: Verdict }) {
  if (v === "ok") return <CheckCircle2 size={14} className="shrink-0 text-[var(--color-good)]" />;
  if (v === "bad") return <XCircle size={14} className="shrink-0 text-[var(--color-bad)]" />;
  if (v === "warn") return <AlertTriangle size={14} className="shrink-0 text-[var(--color-warn)]" />;
  return <CircleHelp size={14} className="shrink-0 text-[var(--color-muted)]" />;
}

function Badge({ children, tone = "muted" }: { children: React.ReactNode; tone?: "muted" | "accent" | "warn" }) {
  const cls =
    tone === "accent"
      ? "bg-[var(--color-accent)]/15 text-[var(--color-accent)]"
      : tone === "warn"
        ? "bg-[var(--color-warn)]/15 text-[var(--color-warn)]"
        : "bg-[var(--color-surface-3)] text-[var(--color-muted)]";
  return <span className={`rounded px-1.5 py-0.5 text-xs font-medium ${cls}`}>{children}</span>;
}

export function PackagePanelView({
  inspection: g,
  coverUrl,
  backdropUrl,
  checks,
  actions,
  tab,
  onTab,
}: {
  inspection: GameInspection;
  coverUrl: string | null;
  backdropUrl: string | null;
  checks: PanelChecks;
  actions: PanelAction[];
  tab: Tab;
  onTab: (t: Tab) => void;
}) {
  const tr = useTr();
  const [showAll, setShowAll] = useState(false);

  const typeLabel =
    {
      game: tr("viewer_type_game", undefined, "Game"),
      update: tr("viewer_type_update", undefined, "Update"),
      dlc: tr("viewer_type_dlc", undefined, "DLC"),
      app: tr("viewer_type_app", undefined, "App"),
    }[g.identity.content_type] ?? null;
  const signLabel =
    {
      fake: tr("viewer_sign_fake", undefined, "Fake (FPKG)"),
      retail: tr("viewer_sign_retail", undefined, "Retail"),
      debug: tr("viewer_sign_debug", undefined, "Debug"),
    }[g.authenticity] ?? null;

  const fw = checks.firmware;
  const checkLines: { v: Verdict; text: string }[] = [];
  if (fw.minFw && fw.consoleFw) {
    checkLines.push({
      v: fw.verdict,
      text:
        fw.verdict === "bad"
          ? tr("viewer_fw_bad", { min: fw.minFw, console: fw.consoleFw }, `Needs ${fw.minFw} — this PS5 is ${fw.consoleFw}`)
          : tr("viewer_fw_ok", { min: fw.minFw }, `Runs on this PS5 (needs ${fw.minFw})`),
    });
  }
  const inst = checks.installed;
  if (inst) {
    const ver = inst.installedVer ?? "";
    checkLines.push({
      v: inst.verdict,
      text:
        inst.relation === "not-installed"
          ? tr("viewer_not_installed", undefined, "Not installed")
          : inst.relation === "newer"
            ? tr("viewer_installed_newer", { ver }, `Newer than the installed ${ver}`)
            : inst.relation === "older"
              ? tr("viewer_installed_older", { ver }, `Older than the installed ${ver}`)
              : tr("viewer_installed_same", { ver }, `Already installed (${ver})`),
    });
  }
  if (checks.space && checks.space.verdict !== "unknown") {
    const short = formatBytes(checks.space.shortBy);
    checkLines.push({
      v: checks.space.verdict,
      text:
        checks.space.verdict === "bad"
          ? tr("viewer_space_bad", { short }, `${short} short on the target drive`)
          : tr("viewer_space_ok", undefined, "Fits on the target drive"),
    });
  }

  const rows: [string, string | null][] = [
    [tr("viewer_field_title_id", undefined, "Title ID"), g.identity.title_id || null],
    [tr("viewer_field_content_id", undefined, "Content ID"), g.identity.content_id || null],
    [tr("viewer_field_concept_id", undefined, "Concept ID"), g.identity.concept_id],
    [tr("viewer_field_version", undefined, "Version"), g.specs.app_ver || null],
    [tr("viewer_field_master", undefined, "Master version"), g.specs.master_ver],
    [tr("viewer_field_category", undefined, "Category"), g.identity.category || null],
    [tr("viewer_field_min_fw", undefined, "Minimum firmware"), g.specs.min_fw],
    [tr("viewer_field_sdk", undefined, "SDK"), g.specs.sdk_ver],
    [tr("viewer_field_built", undefined, "Built"), g.specs.build_date],
    [tr("viewer_field_drm", undefined, "DRM"), g.specs.drm],
    [tr("viewer_field_age", undefined, "Age rating"), g.specs.age_rating],
    [tr("viewer_field_languages", undefined, "Languages"), g.specs.languages.length ? g.specs.languages.join(", ") : null],
    [tr("viewer_field_size", undefined, "Size"), formatBytes(g.source.size)],
    [tr("viewer_field_files", undefined, "Files"), g.specs.file_count != null ? String(g.specs.file_count) : null],
  ];

  const params = showAll ? g.params : g.params.filter((p) => CURATED_KEYS.has(p.key));
  const notes = g.change_notes ? changeNotesText(g.change_notes) : "";
  const copyText = g.params.map((p) => `${p.key}: ${p.value}`).join("\n");

  return (
    <div className="grid gap-4 p-4">
      <header className="relative overflow-hidden rounded-lg border border-[var(--color-border)] bg-[var(--color-surface-2)] p-4">
        {backdropUrl && (
          <img
            src={backdropUrl}
            alt=""
            aria-hidden
            className="pointer-events-none absolute inset-0 h-full w-full object-cover opacity-30 blur-xl"
          />
        )}
        <div className="relative flex flex-col gap-4 sm:flex-row">
          <div className="h-40 w-40 shrink-0 overflow-hidden rounded-lg bg-[var(--color-surface-3)]">
            {coverUrl && <img src={coverUrl} alt="" className="h-full w-full object-cover" />}
          </div>
          <div className="min-w-0 flex-1">
            <h2 className="text-lg font-semibold leading-tight">
              {g.identity.title || g.identity.title_id || g.source.path.split(/[\\/]/).pop()}
            </h2>
            <div className="mt-2 flex flex-wrap gap-1.5">
              {g.identity.platform && <Badge tone="accent">{g.identity.platform.toUpperCase()}</Badge>}
              {typeLabel && <Badge>{typeLabel}</Badge>}
              {g.identity.region && <Badge>{g.identity.region}</Badge>}
              {signLabel && <Badge tone={g.authenticity === "fake" ? "warn" : "muted"}>{signLabel}</Badge>}
              <Badge>{g.source.format}</Badge>
              <Badge>{formatBytes(g.source.size)}</Badge>
            </div>
            {checkLines.length > 0 && (
              <ul className="mt-3 grid gap-1 text-sm">
                {checkLines.map((c) => (
                  <li key={c.text} className="flex items-center gap-2">
                    <VerdictIcon v={c.v} />
                    <span>{c.text}</span>
                  </li>
                ))}
              </ul>
            )}
            {actions.length > 0 && (
              <div className="mt-3 flex flex-wrap gap-2">
                {actions.map((a) => (
                  <Button key={a.label} size="sm" variant={a.primary ? "primary" : "secondary"} onClick={a.onClick}>
                    {a.label}
                  </Button>
                ))}
              </div>
            )}
          </div>
        </div>
      </header>

      {g.warnings.length > 0 && (
        <div className="rounded-md border border-[var(--color-warn)] bg-[var(--color-warn)]/10 px-3 py-2 text-sm text-[var(--color-warn)]">
          {g.warnings.map((w) => (
            <div key={w}>{w}</div>
          ))}
        </div>
      )}

      <div role="tablist" className="flex gap-1 overflow-x-auto border-b border-[var(--color-border)]">
        {(["overview", "details"] as const).map((t) => (
          <button
            key={t}
            type="button"
            role="tab"
            aria-selected={tab === t}
            onClick={() => onTab(t)}
            className={`shrink-0 border-b-2 px-3 py-2 text-sm ${
              tab === t
                ? "border-[var(--color-accent)] text-[var(--color-text)]"
                : "border-transparent text-[var(--color-muted)] hover:text-[var(--color-text)]"
            }`}
          >
            {t === "overview"
              ? tr("viewer_tab_overview", undefined, "Overview")
              : tr("viewer_tab_details", undefined, "Details")}
          </button>
        ))}
      </div>

      {tab === "overview" ? (
        <div className="grid gap-4">
          <dl className="grid grid-cols-[max-content_1fr] gap-x-4 gap-y-1.5 text-sm">
            {rows
              .filter(([, v]) => v)
              .map(([k, v]) => (
                <div key={k} className="contents">
                  <dt className="text-[var(--color-muted)]">{k}</dt>
                  <dd className="min-w-0 break-all font-mono text-xs leading-5">{v}</dd>
                </div>
              ))}
          </dl>
          {g.source.parts.length > 1 && (
            <div className="text-sm">
              <div className="mb-1 text-[var(--color-muted)]">{tr("viewer_field_parts", undefined, "Parts")}</div>
              <ul className="grid gap-0.5 font-mono text-xs">
                {g.source.parts.map((p) => (
                  <li key={p.path} className="flex justify-between gap-3">
                    <span className="truncate">{p.path.split(/[\\/]/).pop()}</span>
                    <span className="tabular-nums text-[var(--color-muted)]">{formatBytes(p.size)}</span>
                  </li>
                ))}
              </ul>
            </div>
          )}
          {notes && (
            <div className="text-sm">
              <div className="mb-1 text-[var(--color-muted)]">{tr("viewer_whats_new", undefined, "What's new")}</div>
              <p className="whitespace-pre-wrap rounded-md bg-[var(--color-surface-2)] p-3 text-xs">
                {notes}
              </p>
            </div>
          )}
        </div>
      ) : (
        <div className="grid gap-2">
          <div className="flex items-center justify-between gap-2">
            <label className="flex items-center gap-2 text-sm">
              <input type="checkbox" checked={showAll} onChange={(e) => setShowAll(e.target.checked)} />
              {tr("viewer_show_all", undefined, "Show all")}
            </label>
            <Button
              size="sm"
              variant="ghost"
              leftIcon={<Copy size={12} />}
              onClick={() => void navigator.clipboard?.writeText(copyText).catch(() => {})}
            >
              {tr("viewer_copy", undefined, "Copy details")}
            </Button>
          </div>
          <dl className="grid grid-cols-[max-content_1fr] gap-x-4 gap-y-1 font-mono text-xs">
            {params.map((p) => (
              <div key={p.key} className="contents">
                <dt className="text-[var(--color-muted)]">{p.key}</dt>
                <dd className="min-w-0 break-all">{p.value}</dd>
              </div>
            ))}
          </dl>
        </div>
      )}
    </div>
  );
}

/** The package viewer in a side drawer (full width on a phone). */
export function PackagePanel({
  path,
  host,
  onClose,
  actions = [],
}: {
  path: string | null;
  host: string | null;
  onClose: () => void;
  actions?: PanelAction[];
}) {
  const tr = useTr();
  const [data, setData] = useState<{ token: string; inspection: GameInspection } | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [cover, setCover] = useState<string | null>(null);
  const [backdrop, setBackdrop] = useState<string | null>(null);
  const [tab, setTab] = useState<Tab>("overview");
  const [installed, setInstalled] = useState<PanelChecks["installed"]>(null);
  const [free, setFree] = useState<number | null>(null);
  const latest = useRef<string | null>(null);
  const kernel = useConnectionStore((s) =>
    host ? (s.runtimeByHost[hostOf(host)]?.ps5Kernel ?? null) : null,
  );

  useEffect(() => {
    latest.current = path;
    setData(null);
    setError(null);
    setCover(null);
    setBackdrop(null);
    setInstalled(null);
    setFree(null);
    setTab("overview");
    if (!path) return;
    void gameInspect(path)
      .then(async (r) => {
        if (latest.current !== path) return;
        setData(r);
        const names = new Set(r.inspection.images.map((i) => i.name));
        if (names.has("icon0.png")) {
          void gameInspectImageUrl(r.token, "icon0.png")
            .then((u) => latest.current === path && setCover(u))
            .catch(() => {});
        }
        const bg = names.has("pic0.png") ? "pic0.png" : names.has("pic1.png") ? "pic1.png" : null;
        if (bg) {
          void gameInspectImageUrl(r.token, bg)
            .then((u) => latest.current === path && setBackdrop(u))
            .catch(() => {});
        }
        if (host) {
          const g = r.inspection;
          if (g.identity.content_id) {
            void pkgInstallPreflight(transferAddr(host), g.identity.content_id)
              .then((pre) => {
                if (latest.current !== path || !pre) return;
                const ver = pre.state === "not_installed" ? null : pre.installedVersion;
                setInstalled({ ...installedCheck(g.specs.app_ver, ver), installedVer: ver });
              })
              .catch(() => {});
          }
          void installFreeBytes(transferAddr(host))
            .then((b) => latest.current === path && setFree(b))
            .catch(() => {});
        }
      })
      .catch((e) => {
        if (latest.current === path) setError(e instanceof Error ? e.message : String(e));
      });
  }, [path, host]);

  // The console's own copy, or the online cover, when the source has no icon.
  const online = useTitleInfo(data && !cover ? data.inspection.identity.title_id || null : null);
  const coverUrl = cover ?? online?.coverImageUrl ?? null;

  const checks: PanelChecks = useMemo(
    () => ({
      firmware: firmwareCheck(data?.inspection.specs.min_fw ?? null, parsePS5Firmware(kernel)),
      installed,
      space: data && host ? spaceCheck(data.inspection.source.size, free) : null,
    }),
    [data, kernel, installed, free, host],
  );

  return (
    <Drawer
      open={path != null}
      onClose={onClose}
      side="right"
      width="min(560px, 100vw)"
      title={tr("viewer_open", undefined, "View details")}
    >
      {error ? (
        <ErrorCard title={tr("viewer_open", undefined, "View details")} detail={error} />
      ) : !data ? (
        <div className="flex justify-center p-8">
          <Spinner size={20} />
        </div>
      ) : (
        <PackagePanelView
          inspection={data.inspection}
          coverUrl={coverUrl}
          backdropUrl={backdrop}
          checks={checks}
          actions={actions}
          tab={tab}
          onTab={setTab}
        />
      )}
    </Drawer>
  );
}

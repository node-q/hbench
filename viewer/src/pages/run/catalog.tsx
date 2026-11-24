// The dataset catalog (/api/run/catalog): domain tiles, then a domain's datasets. /run walks through it, and the
// Jobs page shows the tiles while the folder has no jobs.
import { useEffect, useMemo, useState, type ReactNode } from "react";
import { Link } from "react-router";
import "./catalog.css";
import * as Icon from "../../ui/icons";
import { Empty, ErrorBox, SearchField } from "../../ui/kit";
import { enc, useApi, type Resource } from "../../lib/api";
import type { Catalog, CatalogDataset, CatalogDomain } from "../../lib/types";
import type { Source } from "./form";
import { useDetails, type DetailState } from "./details";

/** The pseudo-domain of tasks the registry's catalog doesn't list: a folder, a git task, a dataset by name. */
export const CUSTOM = "custom";

export const useCatalog = () => useApi<Catalog>("/api/run/catalog");
export const domainHref = (id: string) => `/run?domain=${enc(id)}`;

type Glyph = (p: { size?: number }) => ReactNode;
const GLYPHS: Record<string, Glyph> = {
  coding: Icon.Code,
  "data-analysis": Icon.Bars,
  finance: Icon.Coin,
  legal: Icon.Scales,
  healthcare: Icon.Cross,
  "customer-support": Icon.Chat,
  security: Icon.Lock,
  research: Icon.Book,
  "computer-use": Icon.Monitor,
  underwriting: Icon.Shield,
  claims: Icon.Clipboard,
  actuarial: Icon.Chart,
  other: Icon.Grid,
  [CUSTOM]: Icon.Folder,
};
export const domainGlyph = (id: string): Glyph => GLYPHS[id] ?? Icon.Layers;

const CUSTOM_DOMAIN = { id: CUSTOM, title: "Your own tasks", description: "A local folder, a git repository, or a dataset in your own registry." };

const count = (n: number, one: string) => `${n} ${one}${n === 1 ? "" : "s"}`;

/** Step 1: one tile per domain, then the tile for tasks of your own. */
export function DomainGrid({ catalog }: { catalog: Resource<Catalog> }) {
  const { data, error } = catalog;
  return (
    <div className="cat-domains">
      {error && !data && <ErrorBox error={error} title="Couldn't list the registry's datasets" onRetry={catalog.reload} />}
      <div className="cat-tiles" aria-busy={!data && !error}>
        {!data && !error && <Loading />}
        {data ? data.domains.map((d) => <DomainTile key={d.id} domain={d} />) : !error && Array.from({ length: 8 }, (_, i) => <TileSkeleton key={i} />)}
        <Link to={domainHref(CUSTOM)} className="cat-tile cat-tile--custom">
          <span className="cat-tile__icon">
            <Icon.Folder size={20} />
          </span>
          <span className="cat-tile__title">{CUSTOM_DOMAIN.title}</span>
          <span className="cat-tile__desc">{CUSTOM_DOMAIN.description}</span>
          <span className="cat-tile__foot">
            Folder · Git · Registry
            <Icon.ArrowRight size={14} />
          </span>
        </Link>
      </div>
    </div>
  );
}

/** Says, to a screen reader only, that the catalog is on its way. */
const Loading = () => (
  <span className="sr-only" role="status">
    Loading test sets
  </span>
);

/** A bar standing for text still loading; `w` is its width. */
const Bar = ({ w, h }: { w: string; h?: string }) => <span className="cat-skel" style={{ width: w, height: h }} aria-hidden="true" />;

/** A domain tile's shape while the index loads: the same box, its parts as bars. */
function TileSkeleton() {
  return (
    <span className="cat-tile cat-tile--skeleton" aria-hidden="true">
      <Bar w="36px" h="36px" />
      <Bar w="55%" h="1.1em" />
      <span className="cat-skel__lines">
        <Bar w="100%" />
        <Bar w="70%" />
      </span>
      <Bar w="40%" />
    </span>
  );
}

/** A dataset card's shape while the index loads. */
function CardSkeleton() {
  return (
    <span className="cat-card cat-card--skeleton" aria-hidden="true">
      <span className="cat-card__head">
        <Bar w="45%" h="1.1em" />
        <Bar w="4.5em" />
      </span>
      <span className="cat-skel__lines">
        <Bar w="100%" />
        <Bar w="95%" />
        <Bar w="60%" />
      </span>
      <Bar w="35%" />
    </span>
  );
}

function DomainTile({ domain: d }: { domain: CatalogDomain }) {
  const Glyph = domainGlyph(d.id);
  return (
    <Link to={domainHref(d.id)} className="cat-tile" data-empty={d.n_datasets === 0}>
      <span className="cat-tile__icon">
        <Glyph size={20} />
      </span>
      <span className="cat-tile__title">{d.title}</span>
      <span className="cat-tile__desc">{d.description}</span>
      <span className="cat-tile__foot">
        {d.n_datasets ? count(d.n_datasets, "dataset") : "No datasets yet"}
        <Icon.ArrowRight size={14} />
      </span>
    </Link>
  );
}

const CUSTOM_SOURCES: { source: Source; title: string; description: string; glyph: Glyph }[] = [
  { source: "path", title: "Local folder", description: "A task folder, or a folder of tasks, on this machine.", glyph: Icon.Folder },
  { source: "git", title: "Git repository", description: "One task from a git repository, at a commit you choose.", glyph: Icon.Branch },
  { source: "dataset", title: "Dataset by name", description: "A registry dataset the catalog doesn't list, or one in your own registry.", glyph: Icon.Layers },
];

/**
 * Step 2: the datasets of one domain, or the ways to bring your own tasks. `picked` is the dataset the draft
 * already runs, `custom` the custom source it uses.
 */
export function DatasetCatalog({
  catalog,
  domainId,
  picked,
  custom,
  onPick,
  onCustom,
}: {
  catalog: Resource<Catalog>;
  domainId: string;
  picked?: string;
  custom?: Source;
  onPick: (d: CatalogDataset) => void;
  onCustom: (source: Source) => void;
}) {
  const [q, setQ] = useState("");
  const { data, error } = catalog;
  const details = useDetails();
  const isCustom = domainId === CUSTOM;
  const domain = isCustom ? CUSTOM_DOMAIN : data?.domains.find((d) => d.id === domainId);
  const all = useMemo(() => (data?.datasets ?? []).filter((d) => d.domain === domainId), [data, domainId]);
  // Opening a domain loads its datasets' details, a few at a time; each card fills in as its own arrive.
  useEffect(() => details.load(all), [all, details]);
  const shown = all.filter((d) => {
    const s = details.state(d);
    const text = s.status === "ready" ? `${d.name} ${s.details.description} ${s.details.creator ?? ""}` : d.name;
    return q.toLowerCase().split(/\s+/).filter(Boolean).every((t) => text.toLowerCase().includes(t));
  });
  const Glyph = domainGlyph(domainId);
  const back = (
    <Link to="/run?step=domain" className="btn btn--sm">
      <Icon.ArrowLeft size={12} /> All domains
    </Link>
  );

  if (!isCustom && !data) {
    return error ? (
      <div className="cat-list">
        <ErrorBox error={error} title="Couldn't list the registry's datasets" onRetry={catalog.reload} />
        <div className="toolbar">
          {back}
          <Link to={domainHref(CUSTOM)} className="btn btn--sm">
            Use your own tasks
          </Link>
        </div>
      </div>
    ) : (
      <div className="cat-cards" aria-busy="true">
        <Loading />
        {Array.from({ length: 4 }, (_, i) => (
          <CardSkeleton key={i} />
        ))}
      </div>
    );
  }
  if (!domain) {
    return (
      <Empty title="No such domain" action={back}>
        The catalog has no domain called “{domainId}”.
      </Empty>
    );
  }

  return (
    <div className="cat-list">
      <div className="cat-list__head">
        <span className="cat-tile__icon">
          <Glyph size={20} />
        </span>
        <div className="cat-list__title">
          <h2>{domain.title}</h2>
          <p className="muted">{domain.description}</p>
        </div>
        {all.length > 4 && <SearchField value={q} onChange={setQ} placeholder={`Search ${domain.title.toLowerCase()} datasets`} width="min(260px, 100%)" />}
      </div>

      {isCustom ? (
        <div className="cat-cards">
          {CUSTOM_SOURCES.map((s) => (
            <button key={s.source} type="button" className="cat-card" aria-pressed={custom === s.source} onClick={() => onCustom(s.source)}>
              <span className="cat-card__head">
                <s.glyph size={15} />
                <span className="cat-card__name">{s.title}</span>
                {custom === s.source && <span className="chip chip--accent">Selected</span>}
              </span>
              <span className="cat-card__desc">{s.description}</span>
              <span className="cat-card__foot">
                <span />
                <span className="cat-card__go">
                  Choose <Icon.ArrowRight size={12} />
                </span>
              </span>
            </button>
          ))}
        </div>
      ) : !all.length ? (
        <Empty
          title={`No ${domain.title} datasets yet`}
          action={
            <span className="toolbar" style={{ margin: 0 }}>
              <Link to={domainHref(CUSTOM)} className="btn btn--sm btn--primary">
                Use your own tasks
              </Link>
              {back}
            </span>
          }
        >
          The registry has nothing in this domain so far. You can still benchmark an agent on {domain.title.toLowerCase()} tasks of your own.
        </Empty>
      ) : !shown.length ? (
        <Empty
          title="No datasets match"
          action={
            <button className="btn btn--sm" onClick={() => setQ("")}>
              Clear search
            </button>
          }
        />
      ) : (
        <div className="cat-cards" aria-busy={shown.some((d) => details.state(d).status === "loading")}>
          {shown.map((d) => (
            <DatasetCard key={d.name} d={d} state={details.state(d)} picked={picked === d.name} onPick={() => onPick(d)} onRetry={() => details.retry(d)} />
          ))}
        </div>
      )}

      {(isCustom || all.length > 0) && (
        <div className="cat-list__foot">
          {back}
          {!isCustom && <span className="muted">{count(all.length, "dataset")}</span>}
        </div>
      )}
    </div>
  );
}

/**
 * One dataset of a domain: its name and versions from the index at once; its task count, description and credits
 * as bars until they load, or a note and Retry when they didn't.
 */
function DatasetCard({ d, state, picked, onPick, onRetry }: { d: CatalogDataset; state: DetailState; picked: boolean; onPick: () => void; onRetry: () => void }) {
  const ready = state.status === "ready" ? state.details : undefined;
  const versions = d.versions.length > 1 && count(d.versions.length, "version");
  const head = (
    <span className="cat-card__head">
      <span className="cat-card__name mono">{d.name}</span>
      <span className="chip">{d.version}</span>
      {picked && <span className="chip chip--accent">Selected</span>}
      {ready ? <span className="cat-card__tasks num">{count(ready.n_tasks, "task")}</span> : state.status !== "error" && <Bar w="4.5em" />}
    </span>
  );
  const go = (
    <span className="cat-card__go">
      Select <Icon.ArrowRight size={12} />
    </span>
  );
  if (state.status === "error") {
    // Two controls, so the card is a group rather than one button.
    return (
      <div className="cat-card cat-card--failed" role="group" aria-label={d.name} data-dataset={d.name} tabIndex={-1}>
        {head}
        <span className="cat-card__desc cat-card__err">
          The details of this set didn't load. <span className="mono">{state.message}</span>
        </span>
        <span className="cat-card__foot">
          <button type="button" className="btn btn--sm" onClick={onRetry}>
            <Icon.Refresh size={12} /> Retry
          </button>
          <button type="button" className="btn btn--sm btn--ghost cat-card__go" onClick={onPick}>
            Select <Icon.ArrowRight size={12} />
          </button>
        </span>
      </div>
    );
  }
  return (
    <button type="button" className="cat-card" aria-pressed={picked} aria-busy={!ready} onClick={onPick} data-dataset={d.name}>
      {head}
      {ready ? (
        <span className="cat-card__desc">{ready.description}</span>
      ) : (
        <span className="cat-skel__lines">
          <Bar w="100%" />
          <Bar w="95%" />
          <Bar w="60%" />
        </span>
      )}
      <span className="cat-card__foot">
        {ready ? <span className="trunc">{[ready.creator, ready.license, versions].filter(Boolean).join(" · ")}</span> : <Bar w="35%" />}
        {go}
      </span>
    </button>
  );
}

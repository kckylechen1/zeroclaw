// Shared tool-catalog loader. `ToolPicker` and `ToolPermissionGrid` both need
// the same flattened, group-tagged list of agent tools; this is the one
// place that fetches and caches it so the two components (and anything else
// that needs the catalog) stay in sync instead of hitting the network twice.

import { getTools } from "@/lib/api";
import {
  settleToolCatalogResult,
  type CatalogEntry,
} from "./toolCatalog.logic";

export type { CatalogEntry } from "./toolCatalog.logic";

// Process-wide cache so re-mounting a consumer (e.g. reopening the Cron
// modal, or switching config sections) doesn't re-hit the network. Keyed by
// agent alias (`''` = the gateway's default-agent listing): the agent-tools
// half is `getTools(agent)`, so a catalog bound to a specific agent (e.g. a
// channel's owning agent) caches that agent's real scoped catalog separately
// from the default. Each per-agent catalog is effectively static for the
// daemon's lifetime.
const catalogCache = new Map<string, CatalogEntry[]>();
const catalogInflight = new Map<string, Promise<CatalogEntry[]>>();

/** Synchronous cache peek — `null` when nothing has been fetched yet for
 *  this agent. Lets a consumer seed its initial state without waiting on
 *  the `loadToolCatalog` promise when a previous mount already warmed it. */
export function peekToolCatalog(agent?: string): CatalogEntry[] | null {
  return catalogCache.get(agent ?? "") ?? null;
}

export function loadToolCatalog(agent?: string): Promise<CatalogEntry[]> {
  const key = agent ?? "";
  const cached = catalogCache.get(key);
  if (cached) return Promise.resolve(cached);
  const inflight = catalogInflight.get(key);
  if (inflight) return inflight;
  const promise = Promise.allSettled([getTools(agent)])
    .then(([toolsResult]) => {
      const entries = settleToolCatalogResult(toolsResult);
      catalogCache.set(key, entries);
      return entries;
    })
    .finally(() => {
      catalogInflight.delete(key);
    });
  catalogInflight.set(key, promise);
  return promise;
}

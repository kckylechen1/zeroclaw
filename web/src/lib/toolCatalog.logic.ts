import type { ToolSpec, OptionDomain } from '../types/api';

/** A flattened, group-tagged catalog entry. */
export interface CatalogEntry {
  name: string;
  description: string;
  group: 'agent';
  /** JSON Schema for the tool's args (agent tools only; CLI tools omit it). */
  parameters?: unknown;
  /** Declared structured-output schema, when the tool declares one. */
  output?: unknown;
  /** Parameter name -> runtime option domain, for domain-typed params. */
  param_domains?: Record<string, OptionDomain>;
}

export type CatalogSource = 'agent';

export interface CatalogLoadWarning {
  source: CatalogSource;
  message: string;
}

export interface ToolCatalogLoadResult {
  entries: CatalogEntry[];
  warnings: CatalogLoadWarning[];
}

function reasonMessage(reason: unknown): string {
  return reason instanceof Error ? reason.message : String(reason);
}

export function settleToolCatalogResult(
  toolsResult: PromiseSettledResult<ToolSpec[]>,
): ToolCatalogLoadResult {
  if (toolsResult.status === 'rejected') {
    throw new Error(reasonMessage(toolsResult.reason));
  }

  const warnings: CatalogLoadWarning[] = [];

  const tools = toolsResult.status === 'fulfilled' ? toolsResult.value : [];
  const agentEntries: CatalogEntry[] = tools.map((tnt: ToolSpec) => ({
    name: tnt.name,
    description: tnt.description,
    group: 'agent' as const,
    parameters: tnt.parameters,
    output: tnt.output,
    param_domains: tnt.param_domains,
  }));
  return { entries: agentEntries, warnings };
}

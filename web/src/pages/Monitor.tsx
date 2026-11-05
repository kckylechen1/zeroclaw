import { useCallback, useEffect, useRef, useState } from 'react';
import { RefreshCw, Pause, Play } from 'lucide-react';
import type { LogEvent } from '@/lib/api';
import { getLogs } from '@/lib/api';
import { t } from '@/lib/i18n';

/**
 * Brain monitor (#564): the S5 switch timeline. A read-only view over the
 * structured log (`/api/logs`) preset to the turn/wire categories — the
 * same JSONL the gateway persists, no separate store. Turn lifecycle rows
 * (admission → cancel → terminal) carry request/turn identities and sizes,
 * never content.
 */

const TIMELINE_CATEGORIES = ['session', 'tool', 'agent'] as const;
const PAGE_LIMIT = 120;
const MAX_ROWS = 240;
const POLL_MS = 2_000;

function fetchCategory(category: string, sinceTs?: string): Promise<LogEvent[]> {
  return getLogs({
    category,
    limit: PAGE_LIMIT,
    hide_internal: true,
    ...(sinceTs ? { since_ts: sinceTs } : {}),
  })
    .then((res) => res.events)
    .catch(() => []);
}

interface TimelineRow {
  key: string;
  timestamp: string;
  severity: string;
  action: string;
  session: string;
  agent: string;
  requestId: string;
  turnId: string;
  detail: string;
  isTurnLifecycle: boolean;
}

function shortId(id: string | undefined): string {
  if (!id) return '';
  return id.length > 10 ? `${id.slice(0, 8)}…` : id;
}

function toRow(event: LogEvent): TimelineRow {
  const attrs = (event.attributes ?? {}) as Record<string, unknown>;
  const detailParts: string[] = [];
  for (const field of ['status', 'outcome', 'code', 'epoch', 'after_seq']) {
    if (attrs[field] !== undefined && attrs[field] !== null) {
      detailParts.push(`${field}=${String(attrs[field])}`);
    }
  }
  if (typeof attrs.content_bytes === 'number') {
    detailParts.push(`content_bytes=${attrs.content_bytes}`);
  }
  if (typeof attrs.attachments === 'number' && attrs.attachments > 0) {
    detailParts.push(`attachments=${attrs.attachments}`);
  }
  return {
    key: event.id ?? `${event['@timestamp']}-${event.event?.action}-${detailParts.join(',')}`,
    timestamp: event['@timestamp'] ?? '',
    severity: event.severity_text ?? 'INFO',
    action: event.event?.action ?? '',
    session: typeof attrs.session_id === 'string' ? attrs.session_id : '',
    agent: typeof attrs.agent === 'string' ? attrs.agent : '',
    requestId: typeof attrs.request_id === 'string' ? attrs.request_id : '',
    turnId: typeof attrs.turn_id === 'string' ? attrs.turn_id : '',
    detail: detailParts.join(' · '),
    isTurnLifecycle: Boolean(attrs.turn_id || attrs.request_id),
  };
}

function timeOfDay(timestamp: string): string {
  // RFC 3339 UTC; show the clock part for the timeline.
  const match = timestamp.match(/T(\d{2}:\d{2}:\d{2}(?:\.\d+)?)/);
  return match ? match[1]! : timestamp;
}

export default function Monitor() {
  const [rows, setRows] = useState<TimelineRow[]>([]);
  const [loading, setLoading] = useState(true);
  const [auto, setAuto] = useState(true);
  const sinceRef = useRef<string | undefined>(undefined);

  const refresh = useCallback(async () => {
    const batches = await Promise.all(
      TIMELINE_CATEGORIES.map((category) => fetchCategory(category, sinceRef.current)),
    );
    const merged = batches.flat().map(toRow);
    merged.sort((a, b) => (a.timestamp < b.timestamp ? 1 : -1));
    sinceRef.current = merged[0]?.timestamp ?? sinceRef.current;
    setRows(merged.slice(0, MAX_ROWS));
    setLoading(false);
  }, []);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  useEffect(() => {
    if (!auto) return;
    const timer = setInterval(() => void refresh(), POLL_MS);
    return () => clearInterval(timer);
  }, [auto, refresh]);

  return (
    <div className="flex flex-col h-full">
      <header className="flex items-center justify-between px-6 py-4 border-b border-pc-border">
        <div>
          <h1 className="text-lg font-semibold text-pc-text">{t('monitor.title')}</h1>
          <p className="text-xs text-pc-text-muted">{t('monitor.subtitle')}</p>
        </div>
        <div className="flex items-center gap-2">
          <button
            type="button"
            onClick={() => setAuto((prev) => !prev)}
            className="flex items-center gap-1.5 px-3 py-1.5 text-xs rounded-[var(--radius-md)] border border-pc-border bg-pc-elevated text-pc-text hover:border-pc-accent transition-colors"
            aria-label={auto ? t('monitor.pause') : t('monitor.auto')}
            title={auto ? t('monitor.pause') : t('monitor.auto')}
          >
            {auto ? <Pause className="h-3.5 w-3.5" /> : <Play className="h-3.5 w-3.5" />}
            {auto ? t('monitor.pause') : t('monitor.auto')}
          </button>
          <button
            type="button"
            onClick={() => void refresh()}
            className="flex items-center gap-1.5 px-3 py-1.5 text-xs rounded-[var(--radius-md)] border border-pc-border bg-pc-elevated text-pc-text hover:border-pc-accent transition-colors"
            aria-label={t('monitor.refresh')}
          >
            <RefreshCw className={`h-3.5 w-3.5 ${loading ? 'animate-spin' : ''}`} />
            {t('monitor.refresh')}
          </button>
        </div>
      </header>

      <div className="flex-1 overflow-auto px-6 py-4">
        {loading && rows.length === 0 ? (
          <p className="text-sm text-pc-text-muted">{t('monitor.loading')}</p>
        ) : rows.length === 0 ? (
          <p className="text-sm text-pc-text-muted">{t('monitor.no_events')}</p>
        ) : (
          <table className="w-full text-xs">
            <thead>
              <tr className="text-left text-pc-text-muted border-b border-pc-border">
                <th className="py-2 pr-3 font-medium">{t('monitor.col.time')}</th>
                <th className="py-2 pr-3 font-medium">{t('monitor.col.action')}</th>
                <th className="py-2 pr-3 font-medium">{t('monitor.col.surface')}</th>
                <th className="py-2 pr-3 font-medium">{t('monitor.col.turn')}</th>
                <th className="py-2 font-medium">{t('monitor.col.detail')}</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((row) => (
                <tr
                  key={row.key}
                  className={`border-b border-pc-border/50 ${row.isTurnLifecycle ? 'text-pc-text' : 'text-pc-text-muted'}`}
                >
                  <td className="py-1.5 pr-3 font-mono whitespace-nowrap">{timeOfDay(row.timestamp)}</td>
                  <td className="py-1.5 pr-3 whitespace-nowrap">
                    <span
                      className={`inline-block px-1.5 py-0.5 rounded-[var(--radius-sm)] font-medium ${
                        row.severity === 'ERROR' || row.severity === 'FATAL'
                          ? 'bg-red-500/15 text-red-400'
                          : row.isTurnLifecycle
                            ? 'bg-pc-accent/15 text-pc-accent'
                            : 'bg-pc-border/40 text-pc-text-muted'
                      }`}
                    >
                      {row.action}
                    </span>
                  </td>
                  <td className="py-1.5 pr-3 whitespace-nowrap">
                    {row.agent || '—'}
                    {row.session ? <span className="text-pc-text-muted"> · {shortId(row.session)}</span> : null}
                  </td>
                  <td className="py-1.5 pr-3 font-mono whitespace-nowrap text-pc-text-muted">
                    {shortId(row.turnId || row.requestId) || '—'}
                  </td>
                  <td className="py-1.5 font-mono text-pc-text-muted">{row.detail}</td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </div>
    </div>
  );
}

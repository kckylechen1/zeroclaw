import { useEffect, useRef, useState } from 'react';
import { useSearchParams } from 'react-router-dom';
import { Button, ConfirmDialog } from '@/components/ui';
import { useAuth } from '@/hooks/useAuth';
import { t } from '@/lib/i18n';
import { canReword, decisionRequest, reviewFetch, ReviewError, type Candidate, type CandidateHistory, type Decision, type Head, type Inbox, type InboxItem, type Layer, type Revision, type Soul } from '@/lib/review';
const field = 'w-full min-h-11 rounded-md border border-pc-border bg-pc-base p-3 text-pc-text focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[var(--pc-focus)]';
const date = (value: number) => new Date(value * 1000).toLocaleString();
const layers: Layer[] = ['identity', 'principles', 'growth', 'voice'];
function Value({ value }: {
    value: unknown;
}) {
    if (value == null)
        return <p className="text-pc-text-muted">{t('review.unavailable')}</p>;
    if (Array.isArray(value))
        return <ul className="space-y-3">{value.map((item, index) => <li key={index}>
            <Value value={item}/>
            </li>)}</ul>;
    if (typeof value === 'object')
        return <dl className="space-y-3">{Object.entries(value).map(([key, item]) => <div key={key}>
            <dt className="text-xs text-pc-text-muted">{t(`review.${key}`) === `review.${key}` ? key : t(`review.${key}`)}</dt>
            <dd className="mt-1">
            <Value value={item}/>
            </dd>
            </div>)}</dl>;
    return <span className="whitespace-pre-wrap break-words [overflow-wrap:anywhere]">{String(value)}</span>;
}
function VoiceSource({ source }: { source?: NonNullable<Soul['voice']['sources']>[string] }) {
    if (!source) return <span>{t('review.unavailable')}</span>;
    return <span>{t(`review.${source.kind}`)}
        {source.kind === 'stored' && ` · ${t('review.revision')} ${source.revision}`}
        {source.kind === 'persona' && ` · ${source.persona}${source.card ? ` (${source.card})` : ''}`}
    </span>;
}
function ProposalCard({ item, act, busy }: {
    item: InboxItem;
    act: (item: InboxItem, decision: Decision, text: string, scope: string) => void;
    busy: boolean;
}) {
    const [mode, setMode] = useState<'reword' | 'narrow' | null>(null);
    const [text, setText] = useState(item.kind === 'user_model_candidate' ? item.item.statement : item.kind === 'soul_proposal' ? item.item.proposal : '');
    const [session, setSession] = useState(item.kind === 'user_model_candidate' && item.item.scope.startsWith('session:') ? item.item.scope.slice(8) : '');
    if (item.kind === 'reflection_receipt')
        return <article className="py-5 border-b border-pc-border">
        <h3 className="font-medium">{t('review.reflection')} · {item.agent}</h3>
        <p className="text-xs text-pc-text-muted my-2">{date(item.item.ran_at_unix)}</p>
        <p>{t('review.soul')}: {item.item.proposals_created} · {t('review.user_model')}: {item.item.user_model_candidates_created}</p>
        <p className="text-sm break-words">{item.item.outcome}</p>
        </article>;
    const user = item.kind === 'user_model_candidate';
    return <article className="py-6 border-b border-pc-border space-y-4">
    <div className="flex flex-wrap gap-2 text-xs text-pc-text-muted">
    <span>{t(user ? 'review.user_model' : 'review.soul')}</span>
    <span>{user ? item.item.scope : item.agent}</span>
    <span>{t('review.pending')}</span>
    </div>
    <h3 className="text-lg leading-relaxed whitespace-pre-wrap break-words [overflow-wrap:anywhere]">{user ? item.item.statement : item.item.proposal}</h3>
    {!user && <>
        <p className="text-sm text-pc-text-secondary break-words">{item.item.rationale}</p>
        <p className="text-sm">{t(`review.${item.item.layer}`)} {item.item.trait_key && `${t(`review.${item.item.trait_key}`)} → ${item.item.level}`}{item.item.retire_index !== undefined && ` · ${t('review.retire')}`}</p>
        </>}
    <details>
    <summary className="cursor-pointer text-sm py-2">{t('review.evidence')}</summary>
    <div className="text-sm text-pc-text-secondary mt-2">
    <Value value={user ? item.item.evidence : item.item.session_ref ?? t('review.no_evidence')}/>
    </div>
    <p className="text-xs text-pc-text-muted mt-2">{date(item.item.created_at_unix)}</p>
    </details>
    {mode === 'reword' && <label className="block space-y-2">
        <span>{t('review.final_text')}</span>
        <textarea className={field} rows={4} value={text} onChange={e => setText(e.target.value)}/>
        </label>}
    {mode === 'narrow' && <label className="block space-y-2">
        <span>{t('review.session')}</span>
        <input aria-label={t('review.session')} className={field} value={session} onChange={e => setSession(e.target.value)}/>
        <span className="text-sm text-pc-text-muted block">{t('review.narrow_hint')}</span>
        </label>}
    <div className="flex flex-wrap gap-2">
      {mode ? <>
        <Button disabled={busy || (mode === 'reword' ? !text.trim() : !session.trim())} onClick={() => act(item, mode, text, `session:${session.trim()}`)}>{t('review.apply')}</Button>
        <Button variant="ghost" disabled={busy} onClick={() => setMode(null)}>{t('common.cancel')}</Button>
        </> : <>
        <Button disabled={busy} onClick={() => act(item, 'accept', '', '')}>{t('review.accept')}</Button>{canReword(item) && <Button variant="ghost" disabled={busy} onClick={() => setMode('reword')}>{t('review.reword')}</Button>}{user && <Button variant="ghost" disabled={busy} onClick={() => setMode('narrow')}>{t('review.narrow')}</Button>}<Button variant="ghost" disabled={busy} onClick={() => act(item, 'dismiss', '', '')}>{t('review.dismiss')}</Button>
        </>}
    </div>
  </article>;
}
export default function Review() {
    const { pair, token } = useAuth();
    const [params, setParams] = useSearchParams();
    const agent = params.get('agent') ?? '';
    const [agents, setAgents] = useState<string[]>([]);
    const [tab, setTab] = useState<'inbox' | 'profile' | 'user_model'>('inbox');
    const [inbox, setInbox] = useState<Inbox | null>(null);
    const [soul, setSoul] = useState<Soul | null>(null);
    const [heads, setHeads] = useState<Head[]>([]);
    const [history, setHistory] = useState<{
        layer: Layer;
        revisions: Revision[];
    } | null>(null);
    const [pastCandidates, setPastCandidates] = useState<Candidate[]>([]);
    const [candidate, setCandidate] = useState<CandidateHistory | null>(null);
    const [needsPair, setNeedsPair] = useState(!token);
    const [code, setCode] = useState('');
    const [error, setError] = useState('');
    const [notice, setNotice] = useState('');
    const [loading, setLoading] = useState(false);
    const [busy, setBusy] = useState(false);
    const [version, setVersion] = useState(0);
    const [rollback, setRollback] = useState<{
        agent: string;
        layer: Layer;
        revision: number;
        expected: number;
    } | null>(null);
    const generation = useRef(0);
    const inAction = useRef(false);
    const readSequence = useRef(0);
    const report = (err: unknown) => {
        if (err instanceof ReviewError && (err.status === 401 || err.status === 403)) {
            setNeedsPair(true);
            setInbox(null);
            setSoul(null);
            setHeads([]);
            setHistory(null);
            setCandidate(null);
            setPastCandidates([]);
        }
        setError(t(err instanceof ReviewError ? `review.error_${[401, 403, 409, 429, 503].includes(err.status) ? err.status : 'other'}` : 'review.error_other'));
    };
    useEffect(() => {
        const request = ++generation.current;
        const controller = new AbortController();
        setInbox(null);
        setSoul(null);
        setHeads([]);
        setHistory(null);
        setCandidate(null);
        setPastCandidates([]);
        setError('');
        if (!token) {
            setNeedsPair(true);
            return () => { controller.abort(); generation.current++; };
        }
        setLoading(true);
        // An actual owner-only read establishes this page's authority, not token presence.
        reviewFetch<Inbox>(`/api/review/inbox?limit=100${agent ? `&agent=${encodeURIComponent(agent)}` : ''}`, controller.signal)
            .then(async (data) => {
            if (request !== generation.current)
                return;
            setNeedsPair(false);
            setInbox(data);
            const options = await reviewFetch<{
                agents: string[];
            }>('/api/config/agent-options', controller.signal);
            if (request !== generation.current)
                return;
            setAgents(options.agents);
            if (!agent && options.agents.length) {
                setParams({ agent: options.agents[0]! }, { replace: true });
                return;
            }
            if (tab === 'profile' && agent) {
                const value = await reviewFetch<Soul>(`/api/soul?agent=${encodeURIComponent(agent)}`, controller.signal);
                if (request === generation.current)
                    setSoul(value);
            }
            if (tab === 'user_model') {
                const value = await reviewFetch<{
                    heads: Head[];
                }>('/api/user-model/heads', controller.signal);
                if (request === generation.current)
                    setHeads(value.heads);
            }
        }).catch(err => { if (request === generation.current && !controller.signal.aborted)
            report(err); })
            .finally(() => { if (request === generation.current)
            setLoading(false); });
        return () => { controller.abort(); generation.current++; };
    }, [agent, token, tab, version, setParams]);
    const perform = async (work: () => Promise<unknown>, recordDecision = true) => {
        if (inAction.current)
            return;
        inAction.current = true;
        setBusy(true);
        setError('');
        setNotice('');
        const request = generation.current;
        try {
            await work();
            if (request === generation.current) {
                setNotice(recordDecision ? t('review.saved') : '');
                setVersion(v => v + 1);
            }
        }
        catch (err) {
            if (request === generation.current) {
                report(err);
                if (err instanceof ReviewError && err.status === 409) {
                    setNotice(t('review.conflict'));
                    setVersion(v => v + 1);
                }
            }
        }
        finally {
            inAction.current = false;
            setBusy(false);
        }
    };
    const act = (item: InboxItem, decision: Decision, text: string, scope: string) => void perform(() => { const request = decisionRequest(item, decision, text, scope); return reviewFetch(request.path, undefined, request.body); });
    const inspect = async (path: string, apply: (value: unknown) => void) => {
        const request = generation.current;
        const read = ++readSequence.current;
        setError('');
        try {
            const value = await reviewFetch(path);
            if (request === generation.current && read === readSequence.current)
                apply(value);
        }
        catch (err) {
            if (request === generation.current)
                report(err);
        }
    };
    return <div className="max-w-3xl mx-auto p-4 sm:p-8 pb-16 space-y-6 [&_button]:min-h-11 [&_button]:whitespace-normal [&_button:active]:scale-[0.98]">
    <header>
    <p className="text-xs uppercase tracking-widest text-pc-text-muted mb-2">{t('review.owner_space')}</p>
    <h1 className="text-2xl font-semibold">{t('review.title')}</h1>
    <p className="text-pc-text-secondary mt-2">{t('review.subtitle')}</p>
    </header>
    {error && <p role="alert" className="text-status-error border border-pc-border rounded-md p-3">{error}</p>}
    {notice && <p role="status" className="text-pc-text-secondary">{notice}</p>}
    {needsPair ? <form className="space-y-4 border-t border-pc-border pt-6" onSubmit={e => { e.preventDefault(); void perform(async () => { await pair(code.trim()); setCode(''); }, false); }}>
        <h2 className="text-lg font-medium">{t('review.pair')}</h2>
        <p className="text-pc-text-secondary">{t('review.pair_hint')}</p>
        <label className="block space-y-2">
        <span>{t('review.pair_code')}</span>
        <input autoComplete="one-time-code" className={field} value={code} onChange={e => setCode(e.target.value)}/>
        </label>
        <Button type="submit" disabled={busy || !code.trim()}>{t('review.pair')}</Button>
        </form> : <>
      <div className="flex flex-wrap gap-3 items-end">
        <label className="flex-1 min-w-40 space-y-2">
        <span className="text-sm">{t('review.agent')}</span>
        <select className={field} value={agent} disabled={busy} onChange={e => { setParams({ agent: e.target.value }); setNotice(''); }}>{agents.map(a => <option key={a}>{a}</option>)}</select>
        </label>
        <Button variant="ghost" disabled={busy || loading} onClick={() => setVersion(v => v + 1)}>{t('review.refresh')}</Button>
        </div>
      <nav aria-label={t('review.sections')} className="flex flex-wrap gap-2">{(['inbox', 'profile', 'user_model'] as const).map(section => <Button key={section} variant={tab === section ? 'primary' : 'ghost'} disabled={busy} aria-pressed={tab === section} onClick={() => setTab(section)}>{t(`review.${section}`)}</Button>)}</nav>
      {loading && <p role="status">{t('review.loading')}</p>}
      {tab === 'inbox' && inbox && <section>
            <p className="text-sm text-pc-text-muted">{t('review.shared_hint')}</p>{inbox.items.length === 0 && <p className="py-10 text-pc-text-secondary">{t('review.empty')}</p>}{inbox.items.map(item => <ProposalCard key={item.id} item={item} busy={busy} act={act}/>)}{inbox.next_offset !== null && <Button variant="ghost" disabled={busy} onClick={() => void inspect(`/api/review/inbox?limit=100&offset=${inbox.next_offset}${agent ? `&agent=${encodeURIComponent(agent)}` : ''}`, value => setInbox(value as Inbox))}>{t('review.next')}</Button>}</section>}
      {tab === 'profile' && soul && <section className="space-y-8">{layers.map(layer => {
                    const head = layer === 'voice' ? soul.voice.stored : soul[layer];
                    return <article key={layer} className="border-t border-pc-border pt-5 space-y-4">
                    <h2 className="text-xl font-medium">{t(`review.${layer}`)}</h2>{layer === 'voice' ? soul.voice.effective ? <dl className="space-y-4">{Object.entries(soul.voice.effective).map(([dial, level]) => <div key={dial}>
                            <dt className="text-sm text-pc-text-secondary">{t(`review.${dial}`)}</dt>
                            <dd className="font-medium">{t(`review.${level}`)}</dd>
                            <dd className="text-xs text-pc-text-muted">
                            <VoiceSource source={soul.voice.sources?.[dial]}/>
                            </dd>
                            </div>)}</dl> : <p>{t('review.voice_unavailable')}</p> : <Value value={head?.value}/>}{head && <p className="text-xs text-pc-text-muted">{t('review.revision')} {head.revision} · {t(`review.${head.source}`)} · {date(head.created_at_unix)}</p>}<Button variant="ghost" disabled={busy} onClick={() => void inspect(`/api/soul/history?agent=${encodeURIComponent(agent)}&layer=${layer}`, value => setHistory(value as {
                        layer: Layer;
                        revisions: Revision[];
                    }))}>{t('review.history')}</Button>{history?.layer === layer && <div className="space-y-5 pl-3 border-l border-pc-border">{history.revisions.slice().reverse().map(revision => <div key={revision.revision} className="space-y-3">
                            <p className="text-sm">{t('review.revision')} {revision.revision} · {t(`review.${revision.source}`)}</p>
                            <Value value={revision.value}/>{head && revision.revision !== head.revision && <Button variant="ghost" disabled={busy} onClick={() => setRollback({ agent, layer, revision: revision.revision, expected: head.revision })}>{t('review.rollback')}</Button>}</div>)}</div>}</article>;
                })}</section>}
      {tab === 'user_model' && <section className="space-y-5">
            <Button variant="ghost" onClick={() => void inspect('/api/user-model/candidates', value => setPastCandidates((value as {
                candidates: Candidate[];
            }).candidates))}>{t('review.review_history')}</Button>{pastCandidates.map(item => <div key={item.id} className="border-b border-pc-border pb-3 space-y-2">
                <p className="break-words">{item.statement}</p>
                <Button variant="ghost" onClick={() => void inspect(`/api/user-model/candidates/${encodeURIComponent(item.id)}`, value => setCandidate(value as CandidateHistory))}>{t('review.history')}</Button>
                </div>)}<p className="text-sm text-pc-text-muted">{t('review.shared_hint')}</p>{heads.length === 0 && !loading && <p>{t('review.no_heads')}</p>}{heads.map(head => <article key={head.id} className="space-y-3 border-b border-pc-border py-5">
                <h2 className="text-lg break-words">{head.statement}</h2>
                <p className="text-sm text-pc-text-muted">{head.scope} · {t(`review.${head.authority}`)}</p>{head.source_candidate && <Button variant="ghost" onClick={() => void inspect(`/api/user-model/candidates/${encodeURIComponent(head.source_candidate!)}`, value => setCandidate(value as CandidateHistory))}>{t('review.history')}</Button>}</article>)}{candidate && <article className="border border-pc-border p-4 space-y-3">
                <h3 className="font-medium">{t('review.history')}</h3>
                <p>{candidate.candidate.statement}</p>
                <p>{candidate.review_state}</p>
                <Value value={candidate.candidate.evidence}/>{candidate.review_receipts.map(receipt => <p key={receipt.id}>{receipt.action} · {date(receipt.at_unix)} {receipt.note}</p>)}</article>}</section>}
    </>}
    <ConfirmDialog open={rollback !== null} title={t('review.rollback')} message={t('review.rollback_hint')} confirmLabel={t('review.rollback')} onClose={() => setRollback(null)} onConfirm={() => { if (!rollback)
        return; const choice = rollback; setRollback(null); void perform(() => reviewFetch('/api/soul/rollback', undefined, { agent: choice.agent, layer: choice.layer, to_revision: choice.revision, expected_revision: choice.expected })); }}/>
  </div>;
}

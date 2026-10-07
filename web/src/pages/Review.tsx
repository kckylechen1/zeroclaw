import { useEffect, useRef, useState } from 'react';
import { useSearchParams } from 'react-router-dom';
import { Button, ConfirmDialog } from '@/components/ui';
import { useAuth } from '@/hooks/useAuth';
import { t } from '@/lib/i18n';
import { reflectionOutcome, validationMessage, displayedHead, canNarrow, validNarrowScope, canReword, decisionRequest, reviewFetch, ReviewError, type Candidate, type CandidateHistory, type Decision, type Head, type Inbox, type InboxItem, type Layer, type Revision, type Soul } from '@/lib/review';
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
function ProposalCard({ item, act, busy, heads, headsAvailable, canApproveUserModel, stale = false, rejectedOnly = false }: {
    item: InboxItem;
    act: (item: InboxItem, decision: Decision, text: string, scope: string) => void;
    busy: boolean;
    heads: Head[];
    headsAvailable: boolean;
    canApproveUserModel: boolean;
    stale?: boolean;
    rejectedOnly?: boolean;
}) {
    const [mode, setMode] = useState<'reword' | 'narrow' | null>(null);
    const [text, setText] = useState(item.kind === 'user_model_candidate' ? item.item.statement : item.kind === 'soul_proposal' ? item.item.proposal : '');
    const [session, setSession] = useState(item.kind === 'user_model_candidate' && item.item.scope.startsWith('session:') ? item.item.scope.slice(8) : '');
    const editor = useRef<HTMLTextAreaElement | HTMLInputElement | null>(null);
    useEffect(() => { if (mode) editor.current?.focus(); }, [mode]);
    if (item.kind === 'reflection_receipt') {
        const outcome = reflectionOutcome(item.item.outcome);
        return <article className="py-5 border-b border-pc-border">
        <h3 className="font-medium">{t('review.reflection')} · {item.agent}</h3>
        <p className="text-xs text-pc-text-muted my-2">{date(item.item.ran_at_unix)}</p>
        <p>{t('review.soul')}: {item.item.proposals_created} · {t('review.user_model')}: {item.item.user_model_candidates_created}</p>
        <p className="text-sm break-words">{t(outcome.key)}{outcome.detail && `: ${outcome.detail}`}</p>
        </article>;
    }
    const user = item.kind === 'user_model_candidate';
    const approvalBlocked = user && !canApproveUserModel;
    const currentHead = user ? displayedHead(item.item, heads) : undefined;
    return <article className="py-6 border-b border-pc-border space-y-4">
    <div className="flex flex-wrap gap-2 text-xs text-pc-text-muted">
    <span>{t(user ? 'review.user_model' : 'review.soul')}</span>
    <span>{user ? item.item.scope : item.agent}</span>
    <span>{t(rejectedOnly ? 'review.rejected' : 'review.pending')}</span>
    </div>
    <h3 className="text-lg leading-relaxed whitespace-pre-wrap break-words [overflow-wrap:anywhere]">{user ? item.item.statement : item.item.proposal}</h3>
    {user && <div className="space-y-2 text-sm">
        <p>{t('review.kind')}: {t(`review.${item.item.kind}`)} · {t('review.semantic_key')}: <span className="break-all">{item.item.semantic_key}</span></p>
        <p className="font-medium">{t('review.replaces')}</p>
        {currentHead && <div className="space-y-1">
            <p className="whitespace-pre-wrap break-words">{currentHead.statement}</p>
            <p className="text-pc-text-muted break-all">{t(`review.${currentHead.kind}`)} · {currentHead.scope} · {t(`review.${currentHead.authority}`)} · {currentHead.id}</p>
        </div>}
        {!currentHead && <p>{t(headsAvailable ? 'review.no_replacement' : 'review.heads_unavailable')}</p>}
    </div>}
    {!user && <>
        <p className="text-sm text-pc-text-secondary break-words">{item.item.rationale}</p>
        <p className="text-sm">{t(`review.${item.item.layer}`)} {item.item.trait_key && `${t(`review.${item.item.trait_key}`)} → ${t(`review.${item.item.level}`)}`}{item.item.retire_index !== undefined && ` · ${t('review.retire')}`}</p>
        {item.item.growth_kind && <p>{t('review.kind')}: {t(`review.${item.item.growth_kind}`)}</p>}
        {item.item.retire_index !== undefined && <div className="space-y-2">
            <p className="font-medium">{t('review.retire_target')} · {t('review.revision')} {item.item.target_revision ?? t('review.unavailable')}</p>
            {item.item.retire_target ? <><p>{t(`review.${item.item.retire_target.kind}`)}</p><p className="whitespace-pre-wrap break-words">{item.item.retire_target.text}</p></> : <p>{t('review.unavailable')}</p>}
        </div>}
        </>}
    {approvalBlocked && headsAvailable && <p role="status">{t('review.upgrade_head_guard')}</p>}
    {stale && <p role="alert">{t('review.proposal_stale')}</p>}
    <details>
    <summary className="cursor-pointer text-sm py-2 min-h-11 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[var(--pc-focus)]">{t('review.evidence')}</summary>
    <div className="text-sm text-pc-text-secondary mt-2">
    <Value value={user ? item.item.evidence : item.item.session_ref ?? t('review.no_evidence')}/>
    </div>
    <p className="text-xs text-pc-text-muted mt-2">{date(item.item.created_at_unix)}</p>
    </details>
    {mode === 'reword' && <label className="block space-y-2">
        <span>{t('review.final_text')}</span>
        <textarea ref={node => { editor.current = node; }} className={field} rows={4} value={text} onChange={e => setText(e.target.value)}/>
        </label>}
    {mode === 'narrow' && <label className="block space-y-2">
        <span>{t('review.session')}</span>
        <input ref={node => { editor.current = node; }} aria-label={t('review.session')} readOnly={user && item.item.scope !== 'global'} className={field} value={session} onChange={e => setSession(e.target.value)}/>
        <span className="text-sm text-pc-text-muted block">{t(user && item.item.scope !== 'global' ? 'review.same_session_hint' : 'review.narrow_hint')}</span>
        </label>}
    <div className="flex flex-wrap gap-2">
      {mode ? <>
        <Button disabled={busy || approvalBlocked || (mode === 'reword' ? !text.trim() : !user || !validNarrowScope(item.item, `session:${session.trim()}`))} onClick={() => act(item, mode, text, `session:${session.trim()}`)}>{t('review.apply')}</Button>
        <Button variant="ghost" disabled={busy} onClick={() => setMode(null)}>{t('common.cancel')}</Button>
        </> : <>
        {!approvalBlocked && !stale && !rejectedOnly && <Button disabled={busy} onClick={() => act(item, 'accept', '', '')}>{t('review.accept')}</Button>}{!approvalBlocked && !stale && !rejectedOnly && canReword(item) && <Button variant="ghost" disabled={busy} onClick={() => setMode('reword')}>{t('review.reword')}</Button>}{!approvalBlocked && !stale && user && canNarrow(item.item) && <Button variant="ghost" disabled={busy} onClick={() => setMode('narrow')}>{t('review.narrow')}</Button>}{!rejectedOnly && <Button variant="ghost" disabled={busy} onClick={() => act(item, 'dismiss', '', '')}>{t('review.dismiss')}</Button>}
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
    const [headRead, setHeadRead] = useState<{ heads: Head[]; supports_expected_head?: boolean } | null>(null);
    const heads = headRead?.heads ?? [];
    const canApproveUserModel = headRead?.supports_expected_head === true;
    const [history, setHistory] = useState<{
        layer: Layer;
        revisions: Revision[];
    } | null>(null);
    const [pastCandidates, setPastCandidates] = useState<Candidate[]>([]);
    const [candidate, setCandidate] = useState<CandidateHistory | null>(null);
    const [needsPair, setNeedsPair] = useState(!token);
    const [pairingEnabled, setPairingEnabled] = useState<boolean | null>(null);
    const [staleProposals, setStaleProposals] = useState<Set<string>>(new Set());
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
            setHeadRead(null);
            setHistory(null);
            setCandidate(null);
            setPastCandidates([]);
        }
        setError(t(err instanceof ReviewError && err.status === 400 && err.code === 'invalid' ? validationMessage(err) : err instanceof ReviewError ? `review.error_${[401, 403, 409, 429, 503].includes(err.status) ? err.status : 'other'}` : 'review.error_other'));
    };
    useEffect(() => {
        if (!needsPair) return;
        const controller = new AbortController();
        setPairingEnabled(null);
        reviewFetch<{ require_pairing: boolean }>('/health', controller.signal)
            .then(health => { if (!controller.signal.aborted) setPairingEnabled(health.require_pairing === true); })
            .catch(() => { if (!controller.signal.aborted) setError(t('review.error_other')); });
        return () => controller.abort();
    }, [needsPair, version]);
    useEffect(() => {
        const request = ++generation.current;
        const controller = new AbortController();
        setInbox(null);
        setSoul(null);
        setHeadRead(null);
        setHistory(null);
        setCandidate(null);
        setPastCandidates([]);
        setError('');
        if (!token) {
            setNeedsPair(true);
            return () => { controller.abort(); generation.current++; };
        }
        setLoading(true);
        // Resolve stale bookmarks from current options, then verify operator
        // authority with an actual owner-only read, never with token presence.
        reviewFetch<{ agents: string[] }>('/api/config/agent-options', controller.signal)
            .then(async options => {
            if (request !== generation.current) return;
            setAgents(options.agents);
            if ((!agent && options.agents.length) || (agent && !options.agents.includes(agent))) {
                if (agent) setNotice(t('review.agent_changed'));
                setParams(options.agents[0] ? { agent: options.agents[0] } : {}, { replace: true });
                return;
            }
            // Verify authority using only the selected tab's owner-only API.
            if (tab === 'inbox') {
                const data = await reviewFetch<Inbox>(`/api/review/inbox?limit=100${agent ? `&agent=${encodeURIComponent(agent)}` : ''}`, controller.signal);
                if (request !== generation.current) return;
                setNeedsPair(false);
                setInbox(data);
                // Keep healthy Soul/receipts visible if heads fail. A null headRead
                // keeps User Model approvals unavailable until this read succeeds.
                const current = await reviewFetch<{ heads: Head[]; supports_expected_head?: boolean }>('/api/user-model/heads', controller.signal);
                if (request !== generation.current) return;
                setNeedsPair(false);
                setHeadRead(current);
            } else if (tab === 'user_model') {
                const current = await reviewFetch<{ heads: Head[]; supports_expected_head?: boolean }>('/api/user-model/heads', controller.signal);
                if (request !== generation.current) return;
                setNeedsPair(false);
                setHeadRead(current);
            } else if (agent) {
                const value = await reviewFetch<Soul>(`/api/soul?agent=${encodeURIComponent(agent)}`, controller.signal);
                if (request !== generation.current) return;
                setNeedsPair(false);
                setSoul(value);
            }

        }).catch(err => { if (request === generation.current && !controller.signal.aborted)
            report(err); })
            .finally(() => { if (request === generation.current)
            setLoading(false); });
        return () => { controller.abort(); generation.current++; };
    }, [agent, token, tab, version, setParams]);
    const perform = async (work: () => Promise<unknown>, recordDecision = true, onFailure?: (error: unknown) => void) => {
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
                onFailure?.(err);
                if (err instanceof ReviewError && err.status === 400 && err.code === 'invalid') {
                    setNotice(t(validationMessage(err)));
                    setVersion(v => v + 1);
                }
                if (err instanceof ReviewError && err.status === 409) {
                    setNotice(t(err.code === 'head_conflict' ? 'review.head_conflict' : err.code === 'proposal_stale' ? 'review.proposal_stale' : err.code === 'revision_conflict' ? 'review.revision_conflict' : ['proposal_already_resolved', 'candidate_already_reviewed'].includes(err.code ?? '') ? 'review.already_reviewed' : 'review.conflict'));
                    setVersion(v => v + 1);
                }
            }
        }
        finally {
            inAction.current = false;
            setBusy(false);
        }
    };
    const act = (item: InboxItem, decision: Decision, text: string, scope: string) => void perform(() => { if (item.kind === 'user_model_candidate' && decision !== 'dismiss' && !canApproveUserModel) throw new Error('Head guard unavailable'); const request = decisionRequest(item, decision, text, scope, heads); return reviewFetch(request.path, undefined, request.body); }, true, err => {
        if (err instanceof ReviewError && err.code === 'proposal_stale') setStaleProposals(previous => new Set([...previous, item.id]));
    });
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
            if (request === generation.current && read === readSequence.current)
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
    {needsPair ? pairingEnabled !== true ? <section className="space-y-4 border-t border-pc-border pt-6">
        <h2 className="text-lg font-medium">{t('review.owner_access')}</h2>
        <p>{t(pairingEnabled === false ? 'review.pair_disabled' : 'review.pair_checking')}</p>
        {pairingEnabled === false && <code className="block break-words">zeroclaw gateway get-paircode --new</code>}
        <Button variant="ghost" onClick={() => setVersion(v => v + 1)}>{t('review.check_pairing')}</Button>
        </section> : <form className="space-y-4 border-t border-pc-border pt-6" onSubmit={e => { e.preventDefault(); void perform(async () => { await pair(code.trim()); setCode(''); }, false); }}>
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
            <p className="text-sm text-pc-text-muted">{t('review.shared_hint')}</p>{inbox.items.length === 0 && <p className="py-10 text-pc-text-secondary">{t('review.empty')}</p>}{inbox.items.map(item => <ProposalCard key={item.id} item={item} heads={heads} headsAvailable={headRead !== null} canApproveUserModel={canApproveUserModel} busy={busy} act={act} stale={staleProposals.has(item.id)}/>)}{inbox.next_offset !== null && <Button variant="ghost" disabled={busy} onClick={() => void inspect(`/api/review/inbox?limit=100&offset=${inbox.next_offset}${agent ? `&agent=${encodeURIComponent(agent)}` : ''}`, value => setInbox(value as Inbox))}>{t('review.next')}</Button>}</section>}
      {tab === 'profile' && soul && <section className="space-y-8">{layers.map(layer => {
                    const head = layer === 'voice' ? soul.voice.stored : soul[layer];
                    return <article key={layer} className="border-t border-pc-border pt-5 space-y-4">
                    <h2 className="text-xl font-medium">{t(`review.${layer}`)}</h2>{layer === 'voice' ? soul.voice.effective ? <dl className="space-y-4">{Object.entries(soul.voice.effective).map(([dial, level]) => <div key={dial}>
                            <dt className="text-sm text-pc-text-secondary">{t(`review.${dial}`)}</dt>
                            <dd className="font-medium">{t(`review.${level}`)}</dd>
                            <dd className="text-xs text-pc-text-muted">
                            <VoiceSource source={soul.voice.sources?.[dial]}/>
                            </dd>
                            </div>)}</dl> : <div className="space-y-4"><p>{t('review.voice_unavailable')}</p><h3 className="font-medium">{t('review.configured')}</h3><Value value={soul.voice.configured}/><h3 className="font-medium">{t('review.stored')}</h3><Value value={soul.voice.stored?.value}/></div> : <Value value={head?.value}/>}{head && <p className="text-xs text-pc-text-muted">{t('review.revision')} {head.revision} · {t(`review.${head.source}`)} · {date(head.created_at_unix)}</p>}<Button variant="ghost" disabled={busy} onClick={() => void inspect(`/api/soul/history?agent=${encodeURIComponent(agent)}&layer=${layer}`, value => setHistory(value as {
                        layer: Layer;
                        revisions: Revision[];
                    }))}>{t('review.history')}</Button>{history?.layer === layer && <div className="space-y-5 pl-3 border-l border-pc-border">{history.revisions.slice().reverse().map(revision => <div key={revision.revision} className="space-y-3">
                            <p className="text-sm">{t('review.revision')} {revision.revision} · {t(`review.${revision.source}`)}</p>
                            <p className="text-sm">{revision.proposal_id != null && `${t('review.proposal_id')}: ${revision.proposal_id}`}{revision.rolled_back_from != null && ` · ${t('review.rolled_back_from')}: ${revision.rolled_back_from}`}</p>
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
                <p>{t(`review.state_${candidate.review_state}`)}</p>
                {candidate.review_state === 'rejected' && <ProposalCard key={candidate.candidate.id} item={{ id: `user_model:${candidate.candidate.id}`, kind: 'user_model_candidate', item: candidate.candidate }} heads={heads} headsAvailable={headRead !== null} canApproveUserModel={canApproveUserModel} busy={busy} act={act} rejectedOnly/>}
                <Value value={candidate.candidate.evidence}/>{candidate.review_receipts.map(receipt => <p key={receipt.id}>{t(`review.action_${receipt.action}`)} · {date(receipt.at_unix)} {receipt.note}</p>)}</article>}</section>}
    </>}
    <ConfirmDialog open={rollback !== null} title={t('review.rollback')} message={t('review.rollback_hint')} confirmLabel={t('review.rollback')} onClose={() => setRollback(null)} onConfirm={() => { if (!rollback)
        return; const choice = rollback; setRollback(null); void perform(() => reviewFetch('/api/soul/rollback', undefined, { agent: choice.agent, layer: choice.layer, to_revision: choice.revision, expected_revision: choice.expected })); }}/>
  </div>;
}

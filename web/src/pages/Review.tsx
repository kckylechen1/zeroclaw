import { useEffect, useRef, useState } from 'react';
import { useLocation, useNavigate, useSearchParams } from 'react-router-dom';
import { Button, ConfirmDialog } from '@/components/ui';
import { useAuth } from '@/hooks/useAuth';
import { t } from '@/lib/i18n';
import { reflectionOutcome, validationMessage, EXEMPLAR_FIELDS, EXEMPLAR_MAX_BYTES, MAX_EXEMPLARS, exemplarBytes, exemplarProblem, exemplarsRequest, exemplarsSavable, readExemplarDraftState, readExemplars, type ExemplarField, type ToneExemplar, displayedHead, canNarrow, validNarrowScope, canReword, decisionRequest, reviewFetch, loadCandidatePage, appendCandidatePage, ReviewError, anchorRequest, driftIsEmpty, readDrift, signedSteps, type Candidate, type CandidatePage, type CandidateHistory, type Decision, type Head, type Inbox, type InboxItem, type Layer, type Revision, type Soul, type SoulDrift } from '@/lib/review';
const field = 'w-full min-h-11 rounded-md border border-pc-border bg-pc-base p-3 text-pc-text focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[var(--pc-focus)]';
const date = (value: number) => new Date(value * 1000).toLocaleString();
const layers: Layer[] = ['identity', 'principles', 'tone', 'story'];
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
function ToneSource({ source }: { source?: NonNullable<Soul['tone']['sources']>[string] }) {
    if (!source) return <span>{t('review.unavailable')}</span>;
    return <span>{t(`review.${source.kind}`)}
        {source.kind === 'stored' && ` · ${t('review.revision')} ${source.revision}`}
        {source.kind === 'persona' && ` · ${source.persona}${source.card ? ` (${source.card})` : ''}`}
    </span>;
}
// ADR-018 §4.3: how far Tone and Story sit from the owner's anchor. Keep moves
// the anchor to what the owner is looking at; roll back appends revisions equal
// to the anchor. Both are the owner's call and neither happens automatically.
function DriftCard({ drift, busy, onKeep, onRollback }: {
    drift: SoulDrift;
    busy: boolean;
    onKeep: () => void;
    onRollback: () => void;
}) {
    const empty = driftIsEmpty(drift);
    return <section aria-labelledby="review-drift" className="rounded-md border border-pc-border bg-pc-surface p-4 space-y-4">
    <div>
    <h2 id="review-drift" className="text-lg font-medium">{t('review.drift')}</h2>
    <p className="text-sm text-pc-text-secondary mt-1">{t('review.drift_hint')}</p>
    </div>
    <p className="text-sm">{t('review.anchor')} · {t(`review.anchor_${drift.anchor.source}`)} · {date(drift.anchor.set_at_unix)}</p>
    <dl className="space-y-3">{drift.dials.map(dial => <div key={dial.key} className="flex flex-wrap items-baseline gap-x-3">
        <dt className="basis-full text-sm text-pc-text-secondary">{t(`review.${dial.key}`)}</dt>
        <dd className="font-medium">{t(`review.${dial.anchor_level}`)} → {t(`review.${dial.current_level}`)}</dd>
        <dd className="text-sm text-pc-text-muted" aria-label={`${t('review.steps')}: ${signedSteps(dial.steps)}`}>{signedSteps(dial.steps)}</dd>
        </div>)}</dl>
    <div className="space-y-1 text-sm">
    <h3 className="font-medium">{t('review.since_anchor')}</h3>
    <p>{t('review.story_added')}: {drift.story_added}</p>
    <p>{t('review.story_retired')}: {drift.story_retired}</p>
    <p>{t('review.approved_since_anchor')}: {drift.approved_proposals_since_anchor}</p>
    </div>
    {empty && <p className="text-sm text-pc-text-secondary">{t('review.drift_none')}</p>}
    <div className="flex flex-wrap gap-2">
    <Button disabled={busy} onClick={onKeep}>{t('review.keep_anchor')}</Button>
    <Button variant="ghost" disabled={busy || empty} onClick={onRollback}>{t('review.rollback_anchor')}</Button>
    </div>
    </section>;
}
const bytesLabel = (used: number, max: number) => t('review.exemplar_bytes').replace('{used}', String(used)).replace('{max}', String(max));
const exemplarLabel: Record<ExemplarField, string> = { situation: 'review.exemplar_situation', owner_line: 'review.exemplar_owner', agent_line: 'review.exemplar_agent' };
// ADR-018 §3: the owner's examples of how the agent sounds. The editor keeps
// newlines visible (a textarea, never a single-line input) so text pinned from
// the chat can be fixed by hand; the gateway stays the authority on the limits.
function ExemplarEditor({ drafts, prefilled, busy, onChange, onAdd, onRemove, onSave, onCancel }: {
    drafts: ToneExemplar[];
    prefilled: boolean;
    busy: boolean;
    onChange: (index: number, name: ExemplarField, value: string) => void;
    onAdd: () => void;
    onRemove: (index: number) => void;
    onSave: () => void;
    onCancel: () => void;
}) {
    const rows = useRef<(HTMLTextAreaElement | null)[]>([]);
    const [focusRow, setFocusRow] = useState<number | null>(prefilled ? drafts.length - 1 : 0);
    useEffect(() => {
        if (focusRow === null) return;
        rows.current[focusRow]?.focus();
        setFocusRow(null);
    }, [focusRow]);
    const tooMany = drafts.length > MAX_EXEMPLARS;
    return <form aria-label={t('review.exemplar_edit')} className="space-y-4" onSubmit={e => { e.preventDefault(); if (!busy && exemplarsSavable(drafts)) onSave(); }}>
    {prefilled && <p role="status" className="text-sm text-pc-text-secondary">{t('review.exemplar_prefilled')}</p>}
    {drafts.map((draft, index) => <fieldset key={index} className="min-w-0 space-y-3 rounded-md border border-pc-border p-3">
        <legend className="px-1 text-sm font-medium">{t('review.exemplar_item')} {index + 1}</legend>
        {EXEMPLAR_FIELDS.map(name => {
            const problem = exemplarProblem(name, draft[name]);
            const max = EXEMPLAR_MAX_BYTES[name];
            const id = `exemplar-${index}-${name}`;
            return <div key={name} className="space-y-1">
            <label htmlFor={id} className="block text-sm">{t(exemplarLabel[name])}</label>
            <textarea id={id} ref={node => { if (name === 'situation') rows.current[index] = node; }} className={field} rows={2} value={draft[name]} aria-invalid={problem !== null} aria-describedby={`${id}-note`} onChange={e => onChange(index, name, e.target.value)}/>
            <span id={`${id}-note`} className={`text-xs block ${problem ? 'text-status-error' : 'text-pc-text-muted'}`}>{bytesLabel(exemplarBytes(draft[name]), max)}{problem && ` · ${t(`review.exemplar_problem_${problem}`)}`}</span>
            </div>;
        })}
        <Button type="button" variant="ghost" disabled={busy} aria-label={`${t('review.exemplar_remove')} ${index + 1}`} onClick={() => onRemove(index)}>{t('review.exemplar_remove')}</Button>
        </fieldset>)}
    {tooMany && <p role="status" className="text-sm text-status-error">{t('review.exemplars_too_many')}</p>}
    <div className="flex flex-wrap gap-2">
    <Button type="button" variant="ghost" disabled={busy || drafts.length >= MAX_EXEMPLARS} onClick={() => { setFocusRow(drafts.length); onAdd(); }}>{t('review.exemplar_add')}</Button>
    <Button type="submit" disabled={busy || !exemplarsSavable(drafts)}>{t('review.exemplar_save')}</Button>
    <Button type="button" variant="ghost" disabled={busy} onClick={onCancel}>{t('common.cancel')}</Button>
    </div>
    </form>;
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
        {item.item.story_kind && <p>{t('review.kind')}: {t(`review.${item.item.story_kind}`)}</p>}
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
    const location = useLocation();
    const navigate = useNavigate();
    // "Use as tone example" in the chat arrives as router state: in memory only,
    // read once, and dropped from the history entry as soon as the editor opens.
    const [pinned, setPinned] = useState(() => readExemplarDraftState(location.state));
    const [tab, setTab] = useState<'inbox' | 'profile' | 'user_model'>(pinned ? 'profile' : 'inbox');
    const [editing, setEditing] = useState<{
        base: number;
        drafts: ToneExemplar[];
        prefilled: boolean;
    } | null>(null);
    const [inbox, setInbox] = useState<Inbox | null>(null);
    const [soul, setSoul] = useState<Soul | null>(null);
    const [headRead, setHeadRead] = useState<{ heads: Head[]; supports_expected_head?: boolean } | null>(null);
    const heads = headRead?.heads ?? [];
    const canApproveUserModel = headRead?.supports_expected_head === true;
    const [history, setHistory] = useState<{
        layer: Layer;
        revisions: Revision[];
    } | null>(null);
    const [pastCandidates, setPastCandidates] = useState<CandidatePage | null>(null);
    const [historyLoading, setHistoryLoading] = useState(false);
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
    const [driftRead, setDriftRead] = useState<{ agent: string; soul: Soul; drift: SoulDrift } | null>(null);
    const [anchorRollback, setAnchorRollback] = useState(false);
    const generation = useRef(0);
    const inAction = useRef(false);
    const readSequence = useRef(0);
    const candidateRead = useRef(0);
    const candidateRequest = useRef<AbortController | null>(null);
    const resetCandidateHistory = () => {
        candidateRequest.current?.abort();
        candidateRequest.current = null;
        candidateRead.current++;
        setPastCandidates(null);
        setHistoryLoading(false);
    };
    const report = (err: unknown) => {
        if (err instanceof ReviewError && (err.status === 401 || err.status === 403)) {
            setNeedsPair(true);
            setInbox(null);
            setSoul(null);
            setHeadRead(null);
            setHistory(null);
            setCandidate(null);
            resetCandidateHistory();
            setDriftRead(null);
        }
        setError(t(err instanceof ReviewError && err.code === 'pagination_unavailable' ? 'review.upgrade_history' : err instanceof ReviewError && err.status === 400 && err.code === 'invalid' ? validationMessage(err) : err instanceof ReviewError && err.status === 429 && err.code === 'tone_dial_cooldown' ? 'review.dial_cooldown' : err instanceof ReviewError ? `review.error_${[401, 403, 409, 429, 503].includes(err.status) ? err.status : 'other'}` : 'review.error_other'));
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
        resetCandidateHistory();
        setError('');
        if (!token) {
            setNeedsPair(true);
            return () => { controller.abort(); candidateRequest.current?.abort(); generation.current++; };
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
        return () => { controller.abort(); candidateRequest.current?.abort(); generation.current++; };
    }, [agent, token, tab, version, setParams]);
    // The drift card has its own read, so an old gateway or a failing drift
    // route never blocks the inbox or the profile. The Soul is read first: its
    // revisions are what keep and roll back check, so a write landing between
    // the two reads makes the action stale (409) rather than wrong.
    useEffect(() => {
        if (needsPair || !token || !agent || (tab !== 'inbox' && tab !== 'profile')) return;
        const controller = new AbortController();
        void (async () => {
            try {
                const current = await reviewFetch<Soul>(`/api/soul?agent=${encodeURIComponent(agent)}`, controller.signal);
                const drift = readDrift(await reviewFetch<unknown>(`/api/soul/drift?agent=${encodeURIComponent(agent)}`, controller.signal));
                if (!controller.signal.aborted) setDriftRead(drift ? { agent, soul: current, drift } : null);
            }
            catch (err) {
                if (controller.signal.aborted) return;
                setDriftRead(null);
                if (err instanceof ReviewError && (err.status === 401 || err.status === 403)) report(err);
            }
        })();
        return () => controller.abort();
    }, [agent, token, tab, version, needsPair]);
    const perform = async (work: () => Promise<unknown>, recordDecision = true, onFailure?: (error: unknown) => void, savedKey = 'review.saved') => {
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
                setNotice(recordDecision ? t(savedKey) : '');
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
    // An editor belongs to one agent; switching agents drops its draft.
    useEffect(() => { setEditing(null); }, [agent]);
    // Open the editor on the pinned reply once this agent's Soul is read, so the
    // draft is based on the exemplars head the owner is looking at.
    useEffect(() => {
        if (!pinned || !soul || soul.agent !== pinned.agent || agent !== pinned.agent) return;
        setEditing({ base: soul.tone.exemplars?.revision ?? 0, drafts: [...readExemplars(soul), { situation: pinned.situation, owner_line: pinned.owner_line, agent_line: pinned.agent_line }], prefilled: true });
        setPinned(null);
        navigate({ pathname: location.pathname, search: location.search }, { replace: true, state: null });
    }, [pinned, soul, agent, navigate, location.pathname, location.search]);
    const openExemplars = () => {
        if (!soul) return;
        const current = readExemplars(soul);
        setEditing({ base: soul.tone.exemplars?.revision ?? 0, drafts: current.length ? current : [{ situation: '', owner_line: '', agent_line: '' }], prefilled: false });
    };
    const changeDraft = (index: number, name: ExemplarField, value: string) => setEditing(previous => previous && { ...previous, drafts: previous.drafts.map((draft, i) => i === index ? { ...draft, [name]: value } : draft) });
    const saveExemplars = () => {
        if (!editing) return;
        const request = exemplarsRequest(agent, editing.base, editing.drafts);
        void perform(async () => { await reviewFetch(request.path, undefined, request.body, request.method); setEditing(null); }, true, err => {
            if (err instanceof ReviewError && [401, 403, 409].includes(err.status)) setEditing(null);
        }, 'review.exemplars_saved');
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
    const loadCandidates = async (firstPage = false) => {
        if (candidateRequest.current || busy || loading) return;
        const cursor = firstPage ? null : pastCandidates?.next_cursor;
        if (!firstPage && !cursor) return;
        // A new list invalidates detail reads started from the previous list.
        if (firstPage) readSequence.current++;
        const request = generation.current;
        const read = ++candidateRead.current;
        const controller = new AbortController();
        candidateRequest.current = controller;
        setHistoryLoading(true);
        setError('');
        try {
            const page = await loadCandidatePage(cursor ?? null, controller.signal);
            if (request === generation.current && read === candidateRead.current) {
                setPastCandidates(current => firstPage || !current ? page : appendCandidatePage(current, page));
                if (firstPage) setCandidate(null);
            }
        } catch (err) {
            if (request === generation.current && read === candidateRead.current && !controller.signal.aborted) report(err);
        } finally {
            if (candidateRequest.current === controller) {
                candidateRequest.current = null;
                setHistoryLoading(false);
            }
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
      {(tab === 'inbox' || tab === 'profile') && driftRead?.agent === agent && <DriftCard drift={driftRead.drift} busy={busy} onKeep={() => { const request = anchorRequest(driftRead.soul, 'keep'); void perform(() => reviewFetch(request.path, undefined, request.body)); }} onRollback={() => setAnchorRollback(true)}/>}
      {tab === 'inbox' && inbox && <section>
            <p className="text-sm text-pc-text-muted">{t('review.shared_hint')}</p>{inbox.items.length === 0 && <p className="py-10 text-pc-text-secondary">{t('review.empty')}</p>}{inbox.items.map(item => <ProposalCard key={item.id} item={item} heads={heads} headsAvailable={headRead !== null} canApproveUserModel={canApproveUserModel} busy={busy} act={act} stale={staleProposals.has(item.id)}/>)}{inbox.next_offset !== null && <Button variant="ghost" disabled={busy} onClick={() => void inspect(`/api/review/inbox?limit=100&offset=${inbox.next_offset}${agent ? `&agent=${encodeURIComponent(agent)}` : ''}`, value => setInbox(value as Inbox))}>{t('review.next')}</Button>}</section>}
      {tab === 'profile' && soul && <section className="space-y-8">{layers.map(layer => {
                    const head = layer === 'tone' ? soul.tone.stored : soul[layer];
                    return <article key={layer} className="border-t border-pc-border pt-5 space-y-4">
                    <h2 className="text-xl font-medium">{t(`review.${layer}`)}</h2>{layer === 'tone' ? soul.tone.effective ? <dl className="space-y-4">{Object.entries(soul.tone.effective).map(([dial, level]) => <div key={dial}>
                            <dt className="text-sm text-pc-text-secondary">{t(`review.${dial}`)}</dt>
                            <dd className="font-medium">{t(`review.${level}`)}</dd>
                            <dd className="text-xs text-pc-text-muted">
                            <ToneSource source={soul.tone.sources?.[dial]}/>
                            </dd>
                            </div>)}</dl> : <div className="space-y-4"><p>{t('review.tone_unavailable')}</p><h3 className="font-medium">{t('review.configured')}</h3><Value value={soul.tone.configured}/><h3 className="font-medium">{t('review.stored')}</h3><Value value={soul.tone.stored?.value}/></div> : <Value value={head?.value}/>}{head && <p className="text-xs text-pc-text-muted">{t('review.revision')} {head.revision} · {t(`review.${head.source}`)} · {date(head.created_at_unix)}</p>}{layer === 'tone' && <section aria-labelledby="review-exemplars" className="space-y-3">
                    <h3 id="review-exemplars" className="font-medium">{t('review.exemplars')}</h3>
                    <p className="text-sm text-pc-text-secondary">{t('review.exemplars_hint')}</p>
                    {editing ? <ExemplarEditor drafts={editing.drafts} prefilled={editing.prefilled} busy={busy} onChange={changeDraft} onAdd={() => setEditing(previous => previous && { ...previous, drafts: [...previous.drafts, { situation: '', owner_line: '', agent_line: '' }] })} onRemove={index => setEditing(previous => previous && { ...previous, drafts: previous.drafts.filter((_, i) => i !== index) })} onSave={saveExemplars} onCancel={() => setEditing(null)}/> : <>
                    {readExemplars(soul).length === 0 ? <p className="text-pc-text-muted">{t('review.exemplars_none')}</p> : <ol className="space-y-3">{readExemplars(soul).map((exemplar, index) => <li key={index} className="rounded-md border border-pc-border p-3">
                        <dl className="space-y-2">{EXEMPLAR_FIELDS.map(name => <div key={name}>
                            <dt className="text-xs text-pc-text-muted">{t(exemplarLabel[name])}</dt>
                            <dd className="whitespace-pre-wrap break-words [overflow-wrap:anywhere]">{exemplar[name]}</dd>
                            </div>)}</dl>
                        </li>)}</ol>}
                    {soul.tone.exemplars && <p className="text-xs text-pc-text-muted">{t('review.revision')} {soul.tone.exemplars.revision} · {t(`review.${soul.tone.exemplars.source}`)} · {date(soul.tone.exemplars.created_at_unix)}</p>}
                    <Button variant="ghost" disabled={busy} onClick={openExemplars}>{t(readExemplars(soul).length ? 'review.exemplar_edit' : 'review.exemplar_add')}</Button>
                    </>}
                    </section>}<Button variant="ghost" disabled={busy} onClick={() => void inspect(`/api/soul/history?agent=${encodeURIComponent(agent)}&layer=${layer}`, value => setHistory(value as {
                        layer: Layer;
                        revisions: Revision[];
                    }))}>{t('review.history')}</Button>{history?.layer === layer && <div className="space-y-5 pl-3 border-l border-pc-border">{history.revisions.slice().reverse().map(revision => <div key={revision.revision} className="space-y-3">
                            <p className="text-sm">{t('review.revision')} {revision.revision} · {t(`review.${revision.source}`)}</p>
                            <p className="text-sm">{revision.proposal_id != null && `${t('review.proposal_id')}: ${revision.proposal_id}`}{revision.rolled_back_from != null && ` · ${t('review.rolled_back_from')}: ${revision.rolled_back_from}`}</p>
                            <Value value={revision.value}/>{head && revision.revision !== head.revision && <Button variant="ghost" disabled={busy} onClick={() => setRollback({ agent, layer, revision: revision.revision, expected: head.revision })}>{t('review.rollback')}</Button>}</div>)}</div>}</article>;
                })}</section>}
      {tab === 'user_model' && <section className="space-y-5">
            <Button variant="ghost" disabled={busy || loading || historyLoading} onClick={() => void loadCandidates(true)}>{t('review.review_history')}</Button>
            {historyLoading && <p role="status">{t('review.loading')}</p>}
            {pastCandidates?.candidates.length === 0 && <p>{t('review.no_history')}</p>}
            {pastCandidates?.candidates.map((item: Candidate) => <div key={item.id} className="border-b border-pc-border pb-3 space-y-2">
                <p className="break-words">{item.statement}</p>
                <Button variant="ghost" onClick={() => void inspect(`/api/user-model/candidates/${encodeURIComponent(item.id)}`, value => setCandidate(value as CandidateHistory))}>{t('review.history')}</Button>
                </div>)}{pastCandidates?.next_cursor && <Button variant="ghost" disabled={busy || loading || historyLoading} onClick={() => void loadCandidates()}>{t('review.older_candidates')}</Button>}<p className="text-sm text-pc-text-muted">{t('review.shared_hint')}</p>{heads.length === 0 && !loading && <p>{t('review.no_heads')}</p>}{heads.map(head => <article key={head.id} className="space-y-3 border-b border-pc-border py-5">
                <h2 className="text-lg break-words">{head.statement}</h2>
                <p className="text-sm text-pc-text-muted">{head.scope} · {t(`review.${head.authority}`)}</p>{head.source_candidate && <Button variant="ghost" onClick={() => void inspect(`/api/user-model/candidates/${encodeURIComponent(head.source_candidate!)}`, value => setCandidate(value as CandidateHistory))}>{t('review.history')}</Button>}</article>)}{candidate && <article className="border border-pc-border p-4 space-y-3">
                <h3 className="font-medium">{t('review.history')}</h3>
                <p>{candidate.candidate.statement}</p>
                <p>{t(`review.state_${candidate.review_state}`)}</p>
                {candidate.review_state === 'rejected' && <ProposalCard key={candidate.candidate.id} item={{ id: `user_model:${candidate.candidate.id}`, kind: 'user_model_candidate', item: candidate.candidate }} heads={heads} headsAvailable={headRead !== null} canApproveUserModel={canApproveUserModel} busy={busy} act={act} rejectedOnly/>}
                <Value value={candidate.candidate.evidence}/>{candidate.review_receipts.map(receipt => <p key={receipt.id}>{t(`review.action_${receipt.action}`)} · {date(receipt.at_unix)} {receipt.note}</p>)}</article>}</section>}
    </>}
    <ConfirmDialog open={anchorRollback} title={t('review.rollback_anchor')} message={t('review.rollback_anchor_hint')} confirmLabel={t('review.rollback_anchor')} onClose={() => setAnchorRollback(false)} onConfirm={() => { setAnchorRollback(false); if (!driftRead || driftRead.agent !== agent)
        return; const request = anchorRequest(driftRead.soul, 'rollback'); void perform(() => reviewFetch(request.path, undefined, request.body)); }}/>
    <ConfirmDialog open={rollback !== null} title={t('review.rollback')} message={t('review.rollback_hint')} confirmLabel={t('review.rollback')} onClose={() => setRollback(null)} onConfirm={() => { if (!rollback)
        return; const choice = rollback; setRollback(null); void perform(() => reviewFetch('/api/soul/rollback', undefined, { agent: choice.agent, layer: choice.layer, to_revision: choice.revision, expected_revision: choice.expected })); }}/>
  </div>;
}

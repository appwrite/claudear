import { useEffect, useState } from 'react'
import useSWR from 'swr'
import { ExternalLink } from 'lucide-react'
import { fetchSupportDrafts, reviewSupportDraft, type SupportDraft } from '../lib/api'
import { useAuth } from '../lib/auth'
import { parseUTCDate } from '../lib/formatters'
import { PageHeader } from '../components/layout/page-header'
import { CardStackSkeleton } from '../components/shared/page-skeletons'
import { EmptyState } from '../components/shared/empty-state'
import { TimeAgo } from '../components/shared/time-ago'
import { Badge } from '../components/ui/badge'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '../components/ui/card'

export default function SupportPage() {
  const { user } = useAuth()
  const isAdmin = user?.role === 'admin'
  const { data: drafts, error, isLoading, mutate } = useSWR<SupportDraft[]>('support-drafts', fetchSupportDrafts, {
    refreshInterval: 60000,
  })

  return (
    <div>
      <PageHeader
        title="Support"
        description="Suggested answers to Discord support threads. Approved answers are posted to the thread by the threads bot."
      />
      {isLoading && <CardStackSkeleton />}
      {error && !drafts && (
        <p className="text-sm text-destructive py-8 text-center">Could not load drafts. The threads project may be unreachable.</p>
      )}
      {!isLoading && !error && (drafts ?? []).length === 0 && <EmptyState message="No drafts waiting for review" />}
      <div className="space-y-4">
        {(drafts ?? []).map(draft => (
          <DraftCard key={draft.thread_id} draft={draft} isAdmin={isAdmin} onReviewed={() => mutate()} />
        ))}
      </div>
    </div>
  )
}

function DraftCard({ draft, isAdmin, onReviewed }: { draft: SupportDraft; isAdmin: boolean; onReviewed: () => void }) {
  // The revision being edited. A rewritten draft replaces it when there are no
  // local edits; otherwise the reviewer decides, since approving an old
  // revision is refused.
  const [base, setBase] = useState({ answer: draft.answer, revision: draft.updated_at })
  const [answer, setAnswer] = useState(draft.answer)
  const [saving, setSaving] = useState(false)
  const [failed, setFailed] = useState(false)
  const dirty = answer !== base.answer
  const rewritten = draft.updated_at !== base.revision

  useEffect(() => {
    if (rewritten && !dirty) {
      setBase({ answer: draft.answer, revision: draft.updated_at })
      setAnswer(draft.answer)
      setFailed(false)
    }
  }, [rewritten, dirty, draft.answer, draft.updated_at])

  function loadRewrite() {
    setBase({ answer: draft.answer, revision: draft.updated_at })
    setAnswer(draft.answer)
    setFailed(false)
  }

  async function review(status: 'approved' | 'rejected') {
    setSaving(true)
    setFailed(false)
    try {
      await reviewSupportDraft(draft.thread_id, base.revision, status, dirty ? answer : undefined)
    } catch {
      setFailed(true)
    } finally {
      setSaving(false)
      onReviewed()
    }
  }

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2 text-base">
          <a href={draft.url} target="_blank" rel="noreferrer" className="hover:underline flex items-center gap-1.5">
            {draft.title}
            <ExternalLink className="h-3.5 w-3.5 opacity-60" />
          </a>
          {draft.status === 'failed' && <Badge variant="destructive">Failed to post</Badge>}
        </CardTitle>
        <CardDescription>
          Drafted <TimeAgo date={draft.updated_at} />
          {draft.error && <span className="block text-destructive mt-1">{draft.error}</span>}
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        <div className="max-h-72 overflow-y-auto space-y-3 rounded-md border bg-muted/30 p-3">
          {draft.messages.map((message, i) => (
            <div key={i} className="text-sm">
              <div className="flex items-center gap-2 text-xs text-muted-foreground">
                <span className="font-medium text-foreground">{message.author}</span>
                {message.poster && <Badge variant="outline">poster</Badge>}
                <span>{parseUTCDate(message.timestamp).toLocaleString()}</span>
              </div>
              <p className="whitespace-pre-wrap break-words mt-0.5">{message.content}</p>
            </div>
          ))}
        </div>
        {rewritten && dirty && (
          <div className="flex items-center gap-3 rounded-md border border-destructive/40 p-3 text-sm">
            <span>This draft was rewritten after you started editing.</span>
            <button onClick={loadRewrite} className="px-3 py-1.5 border rounded-md text-sm hover:bg-muted">
              Load the new draft
            </button>
          </div>
        )}
        <textarea
          value={answer}
          onChange={e => setAnswer(e.target.value)}
          disabled={!isAdmin || saving}
          aria-label="Suggested answer"
          className="w-full text-sm bg-muted/50 border rounded-md p-3 min-h-[160px] resize-y focus:outline-none focus:ring-2 focus:ring-primary/30 disabled:opacity-70"
        />
        {isAdmin ? (
          <div className="flex items-center gap-3">
            <button
              onClick={() => review('approved')}
              disabled={saving || rewritten || !answer.trim()}
              className="px-3 py-2 bg-primary text-primary-foreground rounded-md text-sm font-medium hover:bg-primary/90 disabled:opacity-50"
            >
              Approve and post
            </button>
            <button
              onClick={() => review('rejected')}
              disabled={saving || rewritten}
              className="px-3 py-2 border rounded-md text-sm hover:bg-muted disabled:opacity-50"
            >
              Reject
            </button>
            {failed && (
              <span className="text-sm text-destructive">
                Could not save the review. The draft may have changed; check it and try again.
              </span>
            )}
          </div>
        ) : (
          <p className="text-sm text-muted-foreground">Only admins can approve answers.</p>
        )}
      </CardContent>
    </Card>
  )
}

import { BlockSkeleton } from '../shared/page-skeletons'
import type { RetrievalUsageRecord } from '../../lib/api'

const SOURCE_LABELS: Record<string, string> = {
  code_chunk: 'Code chunks',
  similar_issue: 'Similar issues',
  discord_chunk: 'Discord discussions',
  qa: 'Reusable Q&A',
}

const SOURCE_ORDER = ['code_chunk', 'similar_issue', 'discord_chunk', 'qa']

function pct(v: number | null): string {
  return v === null || v === undefined ? '--' : `${Math.round(v * 100)}%`
}

interface Props {
  rows: RetrievalUsageRecord[]
  loading: boolean
}

export function RetrievalModalContent({ rows, loading }: Props) {
  if (loading) {
    return <BlockSkeleton className="h-48 w-full" />
  }

  if (rows.length === 0) {
    return (
      <p className="text-sm text-muted-foreground">
        No retrieved chunks were recorded for this attempt. Retrieval recording
        applies to attempts run after this feature was enabled.
      </p>
    )
  }

  const grouped = SOURCE_ORDER.map(kind => ({
    kind,
    label: SOURCE_LABELS[kind] ?? kind,
    items: rows
      .filter(r => r.source_kind === kind)
      .sort((a, b) => a.rank - b.rank),
  })).filter(g => g.items.length > 0)

  return (
    <div className="space-y-6">
      <p className="text-xs text-muted-foreground">
        Every chunk pulled from each retrieval source for this fix attempt.
        <span className="font-medium"> Sim</span> = cosine similarity at
        retrieval, <span className="font-medium">Injected</span> = included in
        the agent's prompt, <span className="font-medium">Quality</span> =
        relevance judge score (when enabled).
      </p>

      {grouped.map(group => (
        <div key={group.kind}>
          <h3 className="text-sm font-semibold mb-2 flex items-center gap-2">
            {group.label}
            <span className="text-xs font-normal text-muted-foreground">
              ({group.items.length})
            </span>
          </h3>
          <div className="overflow-x-auto">
            <table className="w-full text-xs">
              <thead>
                <tr className="text-left text-muted-foreground border-b">
                  <th className="py-1 pr-2 font-medium">#</th>
                  <th className="py-1 pr-2 font-medium">Ref / File</th>
                  <th className="py-1 pr-2 font-medium text-right">Sim</th>
                  <th className="py-1 pr-2 font-medium text-center">Injected</th>
                  <th className="py-1 font-medium text-right">Quality</th>
                </tr>
              </thead>
              <tbody>
                {group.items.map(r => (
                  <tr
                    key={`${r.source_kind}-${r.chunk_ref}`}
                    className="border-b border-border/50"
                  >
                    <td className="py-1 pr-2 tabular-nums text-muted-foreground">
                      {r.rank}
                    </td>
                    <td className="py-1 pr-2 font-mono break-all max-w-[16rem]">
                      {r.file_path ?? r.chunk_ref}
                    </td>
                    <td className="py-1 pr-2 text-right tabular-nums">
                      {pct(r.similarity_score)}
                    </td>
                    <td className="py-1 pr-2 text-center">
                      {r.injected ? '✓' : '—'}
                    </td>
                    <td className="py-1 text-right tabular-nums">
                      {pct(r.quality_score)}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </div>
      ))}
    </div>
  )
}

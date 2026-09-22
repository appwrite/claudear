import { useState } from 'react'
import useSWR from 'swr'
import {
  fetchDiscordChannels,
  fetchDiscordChannelStats,
  type StoredDiscordChannel,
  type DiscordKnowledgebaseStats,
} from '../lib/api'
import { PageHeader } from '../components/layout/page-header'
import { BlockSkeleton, StatsGridSkeleton } from '../components/shared/page-skeletons'
import { StatsCard } from '../components/shared/stats-card'
import { TimeAgo } from '../components/shared/time-ago'
import { DataTable, type Column } from '../components/shared/data-table'
import { Modal } from '../components/shared/modal'
import { Badge } from '../components/ui/badge'
import { formatNumber, formatDate } from '../lib/formatters'
import { Hash, MessagesSquare, Layers, Clock, ExternalLink } from 'lucide-react'

function channelUrl(c: StoredDiscordChannel): string | null {
  if (!c.guild_id) return null
  return `https://discord.com/channels/${c.guild_id}/${c.channel_id}`
}

function KindBadge({ kind, archived }: { kind: string; archived: boolean }) {
  if (kind === 'thread') {
    return (
      <Badge variant={archived ? 'outline' : 'secondary'}>
        {archived ? 'Archived Thread' : 'Thread'}
      </Badge>
    )
  }
  return <Badge variant="secondary">Channel</Badge>
}

function StatusBadge({ complete }: { complete: boolean }) {
  return complete ? (
    <Badge variant="default">Fully indexed</Badge>
  ) : (
    <Badge variant="outline">Backfilling</Badge>
  )
}

/** Renders the indexed window "from – to" using message timestamps. */
function IndexedWindow({ from, to }: { from: string | null; to: string | null }) {
  if (!from && !to) {
    return <span className="text-muted-foreground text-sm">Not indexed yet</span>
  }
  return (
    <span className="text-sm">
      {from ? formatDate(from) : '--'}
      <span className="text-muted-foreground mx-1.5">→</span>
      {to ? formatDate(to) : '--'}
    </span>
  )
}

export default function ChannelsPage() {
  const [selected, setSelected] = useState<StoredDiscordChannel | null>(null)

  const { data: channels, isLoading: channelsLoading } = useSWR<StoredDiscordChannel[]>(
    'discord-channels',
    fetchDiscordChannels,
    { refreshInterval: 30000 },
  )

  const { data: stats, isLoading: statsLoading } = useSWR<DiscordKnowledgebaseStats>(
    'discord-channel-stats',
    fetchDiscordChannelStats,
    { refreshInterval: 30000 },
  )

  const channelsData = Array.isArray(channels) ? channels : null
  const statsData =
    stats &&
    typeof stats === 'object' &&
    typeof (stats as Partial<DiscordKnowledgebaseStats>).channel_count === 'number'
      ? stats
      : null

  const columns: Column<StoredDiscordChannel>[] = [
    {
      key: 'name',
      header: 'Name',
      render: row => (
        <span className="font-medium">{row.name || row.channel_id}</span>
      ),
    },
    {
      key: 'category_name',
      header: 'Category',
      render: row =>
        row.category_name ? (
          <span className="text-sm">{row.category_name}</span>
        ) : (
          <span className="text-muted-foreground text-sm">--</span>
        ),
    },
    {
      key: 'kind',
      header: 'Type',
      render: row => <KindBadge kind={row.kind} archived={row.archived} />,
    },
    {
      key: 'backfill_complete',
      header: 'Status',
      render: row => <StatusBadge complete={row.backfill_complete} />,
    },
    {
      key: 'indexed_window',
      header: 'Indexed Window',
      render: row => <IndexedWindow from={row.indexed_from} to={row.indexed_to} />,
    },
    {
      key: 'chunk_count',
      header: 'Chunks',
      render: row => <span className="text-sm">{formatNumber(row.chunk_count)}</span>,
      sortable: true,
    },
    {
      key: 'last_indexed_at',
      header: 'Last Indexed',
      render: row =>
        row.last_indexed_at ? (
          <TimeAgo date={row.last_indexed_at} />
        ) : (
          <span className="text-muted-foreground text-sm">--</span>
        ),
      sortable: true,
    },
  ]

  return (
    <div className="space-y-6">
      <PageHeader
        title="Channels"
        description="Discord channels indexed into the knowledgebase"
      />

      {statsLoading && <StatsGridSkeleton count={4} className="md:grid-cols-4" />}

      {statsData && (
        <div className="grid gap-4 md:grid-cols-4">
          <StatsCard
            title="Channels"
            value={statsData.channel_count}
            icon={<Hash className="h-4 w-4 text-primary" />}
            description="Indexed channels"
          />
          <StatsCard
            title="Threads"
            value={statsData.thread_count}
            icon={<MessagesSquare className="h-4 w-4 text-blue-500" />}
            description="Indexed threads"
          />
          <StatsCard
            title="Chunks"
            value={formatNumber(statsData.chunk_count)}
            icon={<Layers className="h-4 w-4 text-green-500" />}
            description="Total message chunks"
          />
          <StatsCard
            title="Last Indexed At"
            value={statsData.last_indexed_at ? formatDate(statsData.last_indexed_at) : '--'}
            icon={<Clock className="h-4 w-4 text-yellow-500" />}
            description="Most recent index run"
          />
        </div>
      )}

      {channelsLoading && <BlockSkeleton className="h-48 w-full" />}
      {channelsData && (
        <DataTable
          columns={columns}
          data={channelsData}
          keyFn={row => row.channel_id}
          emptyMessage="No Discord channels indexed yet. Enable [knowledgebase.discord] to start indexing."
          onRowClick={row => setSelected(row)}
        />
      )}

      <Modal
        open={!!selected}
        onClose={() => setSelected(null)}
        title={selected?.name || selected?.channel_id}
      >
        {selected && (
          <div className="space-y-4">
            <div className="grid gap-3 sm:grid-cols-2">
              <div>
                <p className="text-sm text-muted-foreground">Type</p>
                <p className="text-sm">
                  <KindBadge kind={selected.kind} archived={selected.archived} />
                </p>
              </div>
              <div>
                <p className="text-sm text-muted-foreground">Status</p>
                <p className="text-sm">
                  <StatusBadge complete={selected.backfill_complete} />
                </p>
              </div>
              <div>
                <p className="text-sm text-muted-foreground">Category</p>
                <p className="text-sm">{selected.category_name ?? '--'}</p>
              </div>
              <div>
                <p className="text-sm text-muted-foreground">Channel ID</p>
                <p className="text-sm font-mono">{selected.channel_id}</p>
              </div>
              <div>
                <p className="text-sm text-muted-foreground">Chunks</p>
                <p className="text-sm">{selected.chunk_count.toLocaleString()}</p>
              </div>
              <div className="sm:col-span-2">
                <p className="text-sm text-muted-foreground">Indexed Window</p>
                <p className="text-sm">
                  <IndexedWindow from={selected.indexed_from} to={selected.indexed_to} />
                </p>
              </div>
              <div>
                <p className="text-sm text-muted-foreground">Newest Indexed Message</p>
                <p className="text-sm font-mono">
                  {selected.last_indexed_message_id ?? '--'}
                </p>
              </div>
              <div>
                <p className="text-sm text-muted-foreground">Last Indexed</p>
                <p className="text-sm">
                  {selected.last_indexed_at ? formatDate(selected.last_indexed_at) : '--'}
                </p>
              </div>
            </div>
            {channelUrl(selected) && (
              <a
                href={channelUrl(selected)!}
                target="_blank"
                rel="noopener noreferrer"
                className="inline-flex items-center gap-1.5 rounded-md border px-3 py-1.5 text-sm font-medium hover:bg-muted transition-colors"
              >
                <ExternalLink className="h-4 w-4" />
                Open in Discord
              </a>
            )}
          </div>
        )}
      </Modal>
    </div>
  )
}

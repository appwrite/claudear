import { Cpu, AlertCircle, Loader2, Download } from 'lucide-react'
import useSWR from 'swr'
import { fetchChatModels, isChatDisabled } from '../../lib/chat'
import { ChatDisabledBadge } from './ChatDisabledNotice'
import { useAuth } from '../../lib/auth'
import { useRouter } from '../../router'

export function ModelStatus() {
  const { user } = useAuth()
  const { navigate } = useRouter()
  const isAdmin = user?.role === 'admin'

  const { data, error } = useSWR('chat-models', fetchChatModels, {
    refreshInterval: (latestData) => latestData?.models?.[0]?.status === 'downloading' ? 2000 : 30000,
  })

  const model = data?.models?.[0]

  if (isChatDisabled(error)) {
    return <ChatDisabledBadge />
  }

  if (error || !model) {
    return (
      <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
        <AlertCircle className="h-3.5 w-3.5 text-destructive" />
        <span>No model</span>
      </div>
    )
  }

  const statusColors: Record<string, string> = {
    ready: 'text-green-500',
    loading: 'text-yellow-500',
    downloading: 'text-blue-500',
    notloaded: 'text-muted-foreground',
    error: 'text-destructive',
  }

  const statusIcons: Record<string, typeof Cpu> = {
    loading: Loader2,
    downloading: Loader2,
  }

  // Show progress bar during download
  if (model.status === 'downloading' && model.download_progress != null) {
    return (
      <div className="flex items-center gap-1.5 text-xs">
        <Loader2 className="h-3.5 w-3.5 text-blue-500 animate-spin" />
        <div className="w-16 h-1.5 bg-muted rounded-full overflow-hidden">
          <div
            className="h-full bg-blue-500 rounded-full transition-all duration-300"
            style={{ width: `${model.download_progress}%` }}
          />
        </div>
        <span className="text-muted-foreground">{model.download_progress}%</span>
      </div>
    )
  }

  // Show download link for error status (admin only)
  if (model.status === 'error' && isAdmin) {
    return (
      <div className="flex items-center gap-1.5 text-xs">
        <AlertCircle className="h-3.5 w-3.5 text-destructive" />
        <button
          onClick={() => navigate('/models')}
          className="text-primary hover:underline flex items-center gap-1"
        >
          <Download className="h-3 w-3" />
          Download
        </button>
      </div>
    )
  }

  const Icon = statusIcons[model.status] ?? Cpu
  const colorClass = statusColors[model.status] ?? 'text-muted-foreground'

  return (
    <div className="flex items-center gap-1.5 text-xs">
      <Icon className={`h-3.5 w-3.5 ${colorClass} ${model.status === 'loading' ? 'animate-spin' : ''}`} />
      <span className="text-muted-foreground truncate max-w-[150px]" title={model.name}>
        {model.name}
      </span>
    </div>
  )
}

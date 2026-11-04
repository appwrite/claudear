import { MessageSquareOff } from 'lucide-react'
import { EmptyState } from '../shared/empty-state'
import { useAuth } from '../../lib/auth'
import { useRouter } from '../../router'

export const CHAT_DISABLED_MESSAGE = 'Chat is disabled. Enable [llm] and [chat] in Config and restart claudear.'

export function ChatDisabledNotice() {
  const { user } = useAuth()
  const { navigate } = useRouter()

  return (
    <EmptyState message={CHAT_DISABLED_MESSAGE}>
      {user?.role === 'admin' && (
        <button
          onClick={() => navigate('/config')}
          className="mt-3 text-sm text-primary hover:underline"
        >
          Open Config
        </button>
      )}
    </EmptyState>
  )
}

export function ChatDisabledBadge() {
  return (
    <div className="flex items-center gap-1.5 text-xs text-muted-foreground" title={CHAT_DISABLED_MESSAGE}>
      <MessageSquareOff className="h-3.5 w-3.5" />
      <span>Chat disabled</span>
    </div>
  )
}

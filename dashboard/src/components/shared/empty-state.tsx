import type { ReactNode } from 'react'
import { Inbox } from 'lucide-react'

export function EmptyState({ message = 'No data available', children }: { message?: string; children?: ReactNode }) {
  return (
    <div className="flex flex-col items-center justify-center py-12 text-muted-foreground">
      <Inbox className="h-12 w-12 mb-3 opacity-50" />
      <p className="text-sm">{message}</p>
      {children}
    </div>
  )
}

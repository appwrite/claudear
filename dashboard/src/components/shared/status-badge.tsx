const statusColors: Record<string, string> = {
  pending: 'bg-yellow-500/10 text-yellow-700 dark:text-yellow-400',
  success: 'bg-green-500/10 text-green-700 dark:text-green-400',
  merged: 'bg-purple-500/10 text-purple-700 dark:text-purple-400',
  closed: 'bg-orange-500/10 text-orange-700 dark:text-orange-400',
  failed: 'bg-red-500/10 text-red-700 dark:text-red-400',
  cannot_fix: 'bg-zinc-500/10 text-zinc-700 dark:text-zinc-400',
  awaiting_release: 'bg-blue-500/10 text-blue-700 dark:text-blue-400',
  monitoring: 'bg-cyan-500/10 text-cyan-700 dark:text-cyan-400',
  resolved: 'bg-green-500/10 text-green-700 dark:text-green-400',
  regressed: 'bg-red-500/10 text-red-700 dark:text-red-400',
  open: 'bg-blue-500/10 text-blue-700 dark:text-blue-400',
  running: 'bg-blue-500/10 text-blue-700 dark:text-blue-400',
  verified: 'bg-green-500/10 text-green-700 dark:text-green-400',
  unverified: 'bg-amber-500/10 text-amber-700 dark:text-amber-400',
  errored: 'bg-rose-500/10 text-rose-700 dark:text-rose-400',
}

export function StatusBadge({ status }: { status: string }) {
  const color = statusColors[status] || 'bg-zinc-500/10 text-zinc-700 dark:text-zinc-400'
  return (
    <span className={`px-2 py-1 rounded text-xs font-medium font-mono ${color}`}>
      {status.replace(/_/g, ' ')}
    </span>
  )
}

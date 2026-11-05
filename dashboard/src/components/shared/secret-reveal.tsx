import { useState } from 'react'
import { Check, Copy } from 'lucide-react'

type CopyState = 'idle' | 'copied' | 'failed'

export function SecretReveal({ secret }: { secret: string }) {
  const [state, setState] = useState<CopyState>('idle')

  const handleCopy = async () => {
    try {
      await navigator.clipboard.writeText(secret)
      setState('copied')
      setTimeout(() => setState('idle'), 2000)
    } catch {
      setState('failed')
    }
  }

  return (
    <div className="space-y-1">
      <div className="flex items-center gap-2">
        <code className="flex-1 px-2 py-1.5 bg-muted rounded font-mono text-xs break-all select-all">{secret}</code>
        <button
          type="button"
          onClick={handleCopy}
          className="flex items-center gap-1 px-2 py-1.5 border rounded-md text-xs hover:bg-muted shrink-0"
          title="Copy"
        >
          {state === 'copied' ? <Check className="h-3.5 w-3.5" /> : <Copy className="h-3.5 w-3.5" />}
          {state === 'copied' ? 'Copied' : 'Copy'}
        </button>
      </div>
      {state === 'failed' && (
        <div className="text-xs text-destructive">
          Couldn't access the clipboard. Select the token above and copy it manually.
        </div>
      )}
    </div>
  )
}

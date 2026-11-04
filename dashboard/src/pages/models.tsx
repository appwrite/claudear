import { useState, useCallback } from 'react'
import useSWR from 'swr'
import { PageHeader } from '../components/layout/page-header'
import { useAuth } from '../lib/auth'
import { fetchChatModels, isChatDisabled } from '../lib/chat'
import { ChatDisabledNotice } from '../components/chat/ChatDisabledNotice'
import { browseModels, downloadModel, type BrowseModel, type BrowseResponse } from '../lib/models'
import {
  Search, Download, Loader2, Package, HardDrive,
  AlertCircle, Cpu,
} from 'lucide-react'

function formatBytes(bytes: number): string {
  if (bytes === 0) return '0 B'
  const k = 1024
  const sizes = ['B', 'KB', 'MB', 'GB', 'TB']
  const i = Math.floor(Math.log(bytes) / Math.log(k))
  return `${(bytes / Math.pow(k, i)).toFixed(1)} ${sizes[i]}`
}

export default function ModelsPage() {
  const { user } = useAuth()
  const isAdmin = user?.role === 'admin'

  // Current model status
  const { data: modelsData, error: modelsError, mutate: mutateModels } = useSWR('chat-models', fetchChatModels, {
    refreshInterval: 3000,
  })
  const currentModel = modelsData?.models?.[0]

  // Browse state
  const [searchQuery, setSearchQuery] = useState('')
  const [submittedQuery, setSubmittedQuery] = useState('')
  const [allModels, setAllModels] = useState<BrowseModel[]>([])
  const [nextCursor, setNextCursor] = useState<string | undefined>()
  const [hasSearched, setHasSearched] = useState(false)

  // Download state
  const [downloading, setDownloading] = useState(false)
  const [downloadError, setDownloadError] = useState<string | null>(null)

  const { data: browseData, isLoading: isBrowsing } = useSWR(
    hasSearched ? `browse-models-${submittedQuery}` : null,
    async () => {
      const result = await browseModels(submittedQuery)
      setAllModels(result.models)
      setNextCursor(result.next_cursor)
      return result
    },
  )

  const handleSearch = useCallback((e: React.FormEvent) => {
    e.preventDefault()
    setSubmittedQuery(searchQuery)
    setAllModels([])
    setNextCursor(undefined)
    setHasSearched(true)
  }, [searchQuery])

  const handleLoadMore = useCallback(async () => {
    if (!nextCursor) return
    const result: BrowseResponse = await browseModels(submittedQuery, nextCursor)
    setAllModels(prev => [...prev, ...result.models])
    setNextCursor(result.next_cursor)
  }, [submittedQuery, nextCursor])

  const handleDownloadDefault = useCallback(async () => {
    setDownloading(true)
    setDownloadError(null)
    try {
      await downloadModel()
      mutateModels()
    } catch (err) {
      setDownloadError(err instanceof Error ? err.message : 'Download failed')
    } finally {
      setDownloading(false)
    }
  }, [mutateModels])

  const handleDownloadModel = useCallback(async (model: BrowseModel) => {
    // Find first GGUF file URL
    const url = `https://huggingface.co/${model.name}/resolve/main/${model.name.split('/').pop()?.toLowerCase().replace(/[^a-z0-9]/g, '-')}.gguf`
    // Use the model name's GGUF files — we'll use the API default which points to the config URL
    setDownloading(true)
    setDownloadError(null)
    try {
      await downloadModel(url)
      mutateModels()
    } catch (err) {
      setDownloadError(err instanceof Error ? err.message : 'Download failed')
    } finally {
      setDownloading(false)
    }
  }, [mutateModels])

  const statusColors: Record<string, string> = {
    ready: 'text-green-500',
    loading: 'text-yellow-500',
    downloading: 'text-blue-500',
    notloaded: 'text-muted-foreground',
    error: 'text-destructive',
  }

  const statusLabels: Record<string, string> = {
    ready: 'Ready',
    loading: 'Loading',
    downloading: 'Downloading',
    notloaded: 'Not Loaded',
    error: 'Not Found',
  }

  const models = allModels.length > 0 ? allModels : (browseData?.models ?? [])

  return (
    <div>
      <PageHeader title="Models" description="Browse and manage GGUF models for local inference" />

      {/* Current Model Status */}
      <div className="mb-8 rounded-lg border bg-card p-6">
        <h3 className="text-sm font-semibold uppercase tracking-wider text-muted-foreground mb-4">
          Current Model
        </h3>
        {isChatDisabled(modelsError) ? (
          <ChatDisabledNotice />
        ) : currentModel ? (
          <div className="flex items-center justify-between">
            <div className="flex items-center gap-3">
              <Cpu className={`h-5 w-5 ${statusColors[currentModel.status] ?? 'text-muted-foreground'}`} />
              <div>
                <div className="font-medium">{currentModel.name}</div>
                <div className="text-sm text-muted-foreground">
                  {statusLabels[currentModel.status] ?? currentModel.status}
                  {currentModel.context_length > 0 && ` · ${currentModel.context_length.toLocaleString()} ctx`}
                </div>
              </div>
            </div>
            <div className="flex items-center gap-3">
              {currentModel.status === 'downloading' && currentModel.download_progress != null && (
                <div className="flex items-center gap-2">
                  <div className="w-32 h-2 bg-muted rounded-full overflow-hidden">
                    <div
                      className="h-full bg-blue-500 rounded-full transition-all duration-300"
                      style={{ width: `${currentModel.download_progress}%` }}
                    />
                  </div>
                  <span className="text-xs text-muted-foreground">{currentModel.download_progress}%</span>
                </div>
              )}
              {currentModel.status === 'error' && isAdmin && !downloading && (
                <button
                  onClick={handleDownloadDefault}
                  className="inline-flex items-center gap-1.5 rounded-md bg-primary px-3 py-1.5 text-sm font-medium text-primary-foreground hover:bg-primary/90"
                >
                  <Download className="h-4 w-4" />
                  Download Default
                </button>
              )}
            </div>
          </div>
        ) : (
          <div className="text-sm text-muted-foreground">No model configured</div>
        )}

        {downloadError && (
          <div className="mt-3 text-sm text-destructive flex items-center gap-1.5">
            <AlertCircle className="h-4 w-4" />
            {downloadError}
          </div>
        )}
      </div>

      {/* Browse HuggingFace Models */}
      <div className="rounded-lg border bg-card p-6">
        <h3 className="text-sm font-semibold uppercase tracking-wider text-muted-foreground mb-4">
          Browse HuggingFace GGUF Models
        </h3>

        <form onSubmit={handleSearch} className="flex gap-2 mb-4">
          <div className="relative flex-1">
            <Search className="absolute left-3 top-1/2 -translate-y-1/2 h-4 w-4 text-muted-foreground" />
            <input
              type="text"
              value={searchQuery}
              onChange={(e) => setSearchQuery(e.target.value)}
              placeholder="Search models (e.g., qwen, llama, codestral)..."
              className="w-full rounded-md border bg-background pl-9 pr-3 py-2 text-sm placeholder:text-muted-foreground focus:outline-none focus:ring-2 focus:ring-ring"
            />
          </div>
          <button
            type="submit"
            disabled={isBrowsing}
            className="inline-flex items-center gap-1.5 rounded-md bg-primary px-4 py-2 text-sm font-medium text-primary-foreground hover:bg-primary/90 disabled:opacity-50"
          >
            {isBrowsing ? <Loader2 className="h-4 w-4 animate-spin" /> : <Search className="h-4 w-4" />}
            Search
          </button>
        </form>

        {!hasSearched ? (
          <div className="text-center py-12 text-muted-foreground">
            <Package className="h-12 w-12 mx-auto mb-3 opacity-40" />
            <p className="text-sm">Search HuggingFace to browse available GGUF models</p>
          </div>
        ) : isBrowsing && models.length === 0 ? (
          <div className="text-center py-12">
            <Loader2 className="h-8 w-8 mx-auto mb-3 animate-spin text-muted-foreground" />
            <p className="text-sm text-muted-foreground">Searching models...</p>
          </div>
        ) : models.length === 0 ? (
          <div className="text-center py-12 text-muted-foreground">
            <p className="text-sm">No GGUF models found</p>
          </div>
        ) : (
          <>
            <div className="overflow-auto max-h-[600px] space-y-1">
              {models.map((model) => (
                <div
                  key={model.name}
                  className="flex items-center justify-between rounded-md border p-3 hover:bg-muted/50 transition-colors"
                >
                  <div className="min-w-0 flex-1">
                    <div className="font-medium text-sm truncate">{model.name}</div>
                    <div className="flex items-center gap-2 mt-1 flex-wrap">
                      {model.details?.family && (
                        <span className="inline-flex items-center rounded-full bg-blue-500/10 px-2 py-0.5 text-xs text-blue-600 dark:text-blue-400">
                          {model.details.family}
                        </span>
                      )}
                      {model.details?.parameter_size && (
                        <span className="inline-flex items-center rounded-full bg-purple-500/10 px-2 py-0.5 text-xs text-purple-600 dark:text-purple-400">
                          {model.details.parameter_size}
                        </span>
                      )}
                      {model.details?.quantization_level && (
                        <span className="inline-flex items-center rounded-full bg-amber-500/10 px-2 py-0.5 text-xs text-amber-600 dark:text-amber-400">
                          {model.details.quantization_level}
                        </span>
                      )}
                      {model.size && (
                        <span className="inline-flex items-center gap-1 text-xs text-muted-foreground">
                          <HardDrive className="h-3 w-3" />
                          {formatBytes(model.size)}
                        </span>
                      )}
                    </div>
                  </div>
                  {isAdmin && (
                    <button
                      onClick={() => handleDownloadModel(model)}
                      disabled={downloading}
                      className="ml-3 shrink-0 inline-flex items-center gap-1 rounded-md border px-2.5 py-1.5 text-xs font-medium hover:bg-muted disabled:opacity-50"
                      title={`Download ${model.name}`}
                    >
                      <Download className="h-3.5 w-3.5" />
                      Install
                    </button>
                  )}
                </div>
              ))}
            </div>

            {nextCursor && (
              <button
                onClick={handleLoadMore}
                className="mt-3 w-full rounded-md border py-2 text-sm text-muted-foreground hover:bg-muted transition-colors"
              >
                Load more
              </button>
            )}
          </>
        )}
      </div>
    </div>
  )
}

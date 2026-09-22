import { getCsrfToken } from './api'

const API_BASE = '/api'

export interface ModelDetails {
  format?: string
  family?: string
  parameter_size?: string
  quantization_level?: string
}

export interface BrowseModel {
  name: string
  size?: number
  digest?: string
  modified_at?: string
  details?: ModelDetails
}

export interface BrowseResponse {
  models: BrowseModel[]
  next_cursor?: string
}

export interface ModelInfoResponse {
  name: string
  gguf_size?: number
  details?: ModelDetails
}

export async function browseModels(
  query = '',
  cursor?: string,
  limit = 20
): Promise<BrowseResponse> {
  const params = new URLSearchParams()
  if (query) params.set('q', query)
  if (cursor) params.set('cursor', cursor)
  params.set('limit', String(limit))

  const res = await fetch(`${API_BASE}/chat/models/browse?${params}`)
  if (!res.ok) throw new Error(`Failed to browse models: ${res.statusText}`)
  return res.json()
}

export async function getModelInfo(modelId: string): Promise<ModelInfoResponse> {
  const res = await fetch(`${API_BASE}/chat/models/browse/${encodeURIComponent(modelId)}`)
  if (!res.ok) throw new Error(`Failed to fetch model info: ${res.statusText}`)
  return res.json()
}

export async function downloadModel(url?: string): Promise<void> {
  const res = await fetch(`${API_BASE}/chat/models/download`, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      'x-csrf-token': getCsrfToken(),
    },
    body: JSON.stringify({ url }),
  })
  if (!res.ok) {
    const body = await res.json().catch(() => ({ error: res.statusText }))
    throw new Error(body.error || res.statusText)
  }
}

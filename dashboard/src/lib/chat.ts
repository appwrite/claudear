import { useState, useRef, useCallback, useEffect } from 'react'
import useSWR from 'swr'
import { ApiError, deleteRequest, fetchJson, getWsBase } from './api'

export interface ChatMessage {
  id?: number
  role: 'user' | 'assistant'
  content: string
  sources?: ChatSource[]
  created_at?: string
}

export interface ChatSource {
  file_path: string
  start_line: number
  end_line: number
  symbol_name?: string
  similarity: number
}

export interface ChatSession {
  id: string
  repo_id?: number
  created_at: string
  updated_at: string
  messages?: ChatMessage[]
}

interface ChatChunk {
  delta: string
  done: boolean
  sources?: ChatSource[]
  session_id?: string
  error?: string
  stopped?: boolean
}

interface ChatRequest {
  type: 'chat'
  message: string
  session_id?: string
  repo_id?: number
  max_context_chunks?: number
}

interface UseChatReturn {
  messages: ChatMessage[]
  sendMessage: (message: string) => void
  stopStreaming: () => void
  isStreaming: boolean
  isConnected: boolean
  error: string | null
  sessionId: string | null
  clearMessages: () => void
}

export function useChat(repoId?: number, enabled = true): UseChatReturn {
  const [messages, setMessages] = useState<ChatMessage[]>([])
  const [isStreaming, setIsStreaming] = useState(false)
  const [isConnected, setIsConnected] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [sessionId, setSessionId] = useState<string | null>(null)
  const wsRef = useRef<WebSocket | null>(null)
  const reconnectTimer = useRef<ReturnType<typeof setTimeout>>(undefined)
  const mountedRef = useRef(true)
  const enabledRef = useRef(enabled)
  const streamingContentRef = useRef('')

  useEffect(() => {
    enabledRef.current = enabled
  }, [enabled])

  const connect = useCallback(() => {
    if (!enabledRef.current || !mountedRef.current || wsRef.current?.readyState === WebSocket.OPEN) return

    const ws = new WebSocket(`${getWsBase()}/api/chat/ws`)
    wsRef.current = ws

    ws.onopen = () => {
      if (mountedRef.current) {
        setIsConnected(true)
        setError(null)
      }
    }

    ws.onmessage = (event) => {
      try {
        const chunk: ChatChunk = JSON.parse(event.data)

        if (chunk.error) {
          setError(chunk.error)
          setIsStreaming(false)
          return
        }

        if (chunk.session_id && !sessionId) {
          setSessionId(chunk.session_id)
        }

        // Accumulate streaming content
        streamingContentRef.current += chunk.delta

        // Update the last assistant message with accumulated content
        setMessages(prev => {
          const updated = [...prev]
          const lastIdx = updated.length - 1
          if (lastIdx >= 0 && updated[lastIdx].role === 'assistant') {
            updated[lastIdx] = {
              ...updated[lastIdx],
              content: streamingContentRef.current,
              sources: chunk.done ? chunk.sources : updated[lastIdx].sources,
            }
          }
          return updated
        })

        if (chunk.done) {
          setIsStreaming(false)
          streamingContentRef.current = ''
        }
      } catch { /* ignore malformed messages */ }
    }

    ws.onclose = () => {
      wsRef.current = null
      if (mountedRef.current) {
        setIsConnected(false)
        reconnectTimer.current = setTimeout(connect, 3000)
      }
    }

    ws.onerror = () => ws.close()
  }, [sessionId])

  const sendMessage = useCallback((message: string) => {
    if (!wsRef.current || wsRef.current.readyState !== WebSocket.OPEN) {
      setError('Not connected')
      return
    }

    setError(null)
    setIsStreaming(true)
    streamingContentRef.current = ''

    // Add user message immediately
    const userMsg: ChatMessage = { role: 'user', content: message }
    // Add empty assistant message that will be filled by streaming
    const assistantMsg: ChatMessage = { role: 'assistant', content: '', sources: [] }

    setMessages(prev => [...prev, userMsg, assistantMsg])

    const request: ChatRequest = {
      type: 'chat',
      message,
      session_id: sessionId ?? undefined,
      repo_id: repoId,
    }

    wsRef.current.send(JSON.stringify(request))
  }, [sessionId, repoId])

  const stopStreaming = useCallback(() => {
    if (!wsRef.current || wsRef.current.readyState !== WebSocket.OPEN) return
    wsRef.current.send(JSON.stringify({ type: 'stop' }))
  }, [])

  const clearMessages = useCallback(() => {
    setMessages([])
    setSessionId(null)
    streamingContentRef.current = ''
  }, [])

  useEffect(() => {
    mountedRef.current = true
    if (!enabled) return
    connect()
    return () => {
      mountedRef.current = false
      if (reconnectTimer.current) clearTimeout(reconnectTimer.current)
      wsRef.current?.close()
    }
  }, [connect, enabled])

  return { messages, sendMessage, stopStreaming, isStreaming, isConnected, error, sessionId, clearMessages }
}

const API_BASE = '/api'

export interface ChatModel {
  name: string
  status: string
  context_length: number
  download_progress?: number
}

export interface ChatModelsResponse {
  models: ChatModel[]
}

export interface ChatStatus {
  enabled: boolean
}

export interface ChatAvailability {
  enabled: boolean
  disabled: boolean
  error: unknown
}

export function isChatDisabled(error: unknown): boolean {
  return error instanceof ApiError && error.status === 404
}

export async function fetchChatStatus(): Promise<ChatStatus> {
  return fetchJson(`${API_BASE}/chat/status`)
}

export function useChatAvailability(): ChatAvailability {
  const { data, error } = useSWR<ChatStatus>('chat-status', fetchChatStatus)
  const missing = isChatDisabled(error)
  return {
    enabled: data?.enabled === true,
    disabled: data?.enabled === false || missing,
    error: missing ? undefined : error,
  }
}

export async function fetchChatSessions(): Promise<ChatSession[]> {
  return fetchJson(`${API_BASE}/chat/sessions`)
}

export async function fetchChatSession(id: string): Promise<ChatSession> {
  return fetchJson(`${API_BASE}/chat/sessions/${encodeURIComponent(id)}`)
}

export async function deleteChatSession(id: string): Promise<void> {
  return deleteRequest(`${API_BASE}/chat/sessions/${encodeURIComponent(id)}`)
}

export async function fetchChatModels(): Promise<ChatModelsResponse> {
  return fetchJson(`${API_BASE}/chat/models`)
}

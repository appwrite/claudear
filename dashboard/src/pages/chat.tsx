import { useState, useRef, useEffect, useCallback } from 'react'
import useSWR from 'swr'
import { ChatMessage } from '../components/chat/ChatMessage'
import { ChatInput } from '../components/chat/ChatInput'
import { ChatSessions } from '../components/chat/ChatSessions'
import { ModelStatus } from '../components/chat/ModelStatus'
import { ChatDisabledNotice } from '../components/chat/ChatDisabledNotice'
import { useChat, fetchChatSessions, deleteChatSession, isChatDisabled, type ChatSession } from '../lib/chat'
import { MessageSquareCode, PanelLeftClose, PanelLeft } from 'lucide-react'

export default function ChatPage() {
  const [sidebarOpen, setSidebarOpen] = useState(true)
  const messagesEndRef = useRef<HTMLDivElement>(null)

  const {
    data: sessions,
    error: sessionsError,
    mutate: mutateSessions,
  } = useSWR<ChatSession[]>('chat-sessions', fetchChatSessions, {
    refreshInterval: 10000,
  })
  const disabled = isChatDisabled(sessionsError)
  const available = sessions !== undefined && !disabled
  const failed = !available && !disabled && sessionsError !== undefined

  const { messages, sendMessage, stopStreaming, isStreaming, isConnected, error, sessionId, clearMessages } =
    useChat(undefined, available)

  useEffect(() => {
    messagesEndRef.current?.scrollIntoView({ behavior: 'smooth' })
  }, [messages])

  const handleNewSession = useCallback(() => {
    clearMessages()
  }, [clearMessages])

  const handleSelectSession = useCallback((_id: string) => {
    // TODO: Load session messages when session management is complete
    clearMessages()
  }, [clearMessages])

  const handleDeleteSession = useCallback(async (id: string) => {
    try {
      await deleteChatSession(id)
      mutateSessions()
    } catch (e) {
      console.error('Failed to delete session:', e)
    }
  }, [mutateSessions])

  return (
    <div className="h-[calc(100vh-theme(spacing.14)-theme(spacing.12))] flex flex-col -m-6">
      <div className="flex items-center justify-between px-6 py-3 border-b bg-card">
        <div className="flex items-center gap-3">
          <button
            onClick={() => setSidebarOpen(!sidebarOpen)}
            className="p-1.5 rounded hover:bg-muted text-muted-foreground"
            title={sidebarOpen ? 'Hide sidebar' : 'Show sidebar'}
          >
            {sidebarOpen ? <PanelLeftClose className="h-4 w-4" /> : <PanelLeft className="h-4 w-4" />}
          </button>
          <h1 className="text-lg font-semibold flex items-center gap-2">
            <MessageSquareCode className="h-5 w-5" />
            Chat
          </h1>
          {available && !isConnected && (
            <span className="text-xs text-destructive">Disconnected</span>
          )}
        </div>
        <ModelStatus />
      </div>

      {disabled ? (
        <div className="flex-1 flex items-center justify-center">
          <ChatDisabledNotice />
        </div>
      ) : failed ? (
        <div className="flex-1 flex items-center justify-center">
          <div className="text-destructive text-sm">Failed to load chat sessions.</div>
        </div>
      ) : (
        <div className="flex-1 flex min-h-0">
          {sidebarOpen && (
            <div className="w-56 border-r bg-card shrink-0">
              <ChatSessions
                sessions={sessions ?? []}
                activeSessionId={sessionId}
                onSelectSession={handleSelectSession}
                onNewSession={handleNewSession}
                onDeleteSession={handleDeleteSession}
              />
            </div>
          )}

          <div className="flex-1 flex flex-col min-w-0">
            <div className="flex-1 overflow-y-auto p-6 space-y-4">
              {messages.length === 0 ? (
                <EmptyChat />
              ) : (
                messages.map((msg, i) => (
                  <ChatMessage
                    key={i}
                    message={msg}
                    isStreaming={isStreaming && i === messages.length - 1 && msg.role === 'assistant'}
                  />
                ))
              )}
              {error && (
                <div className="rounded-lg border border-destructive/50 bg-destructive/5 px-4 py-2 text-sm text-destructive">
                  {error}
                </div>
              )}
              <div ref={messagesEndRef} />
            </div>

            <ChatInput
              onSend={sendMessage}
              onStop={stopStreaming}
              isStreaming={isStreaming}
              disabled={!isConnected}
              placeholder={isConnected ? 'Ask about your code...' : 'Connecting...'}
            />
          </div>
        </div>
      )}
    </div>
  )
}

function EmptyChat() {
  return (
    <div className="flex-1 flex items-center justify-center">
      <div className="text-center max-w-sm space-y-3">
        <MessageSquareCode className="h-12 w-12 mx-auto text-muted-foreground/30" />
        <h2 className="text-lg font-medium text-foreground">Ask about your code</h2>
        <p className="text-sm text-muted-foreground">
          Ask questions about your indexed repositories. The assistant uses semantic code search
          to find relevant code and answer your questions.
        </p>
        <div className="text-xs text-muted-foreground space-y-1 pt-2">
          <p>Try asking:</p>
          <div className="space-y-1">
            <div className="inline-block px-2 py-1 rounded bg-muted text-left">
              "What does the main function do?"
            </div>
            <div className="inline-block px-2 py-1 rounded bg-muted text-left">
              "How does error handling work?"
            </div>
            <div className="inline-block px-2 py-1 rounded bg-muted text-left">
              "Show me the database schema"
            </div>
          </div>
        </div>
      </div>
    </div>
  )
}

export const INVITE_PATH = '/invite'

interface InviteHistoryState {
  inviteToken: string
}

function isInviteHistoryState(state: unknown): state is InviteHistoryState {
  return typeof state === 'object'
    && state !== null
    && typeof (state as Partial<InviteHistoryState>).inviteToken === 'string'
}

export function isInvitePath(): boolean {
  const { pathname } = window.location
  return pathname === INVITE_PATH || pathname === `${INVITE_PATH}/`
}

/**
 * Moves the invite secret from the URL fragment into history state so it never
 * reaches Sentry, Referer headers or a copied address bar. Idempotent, and must
 * run before Sentry starts recording the page URL.
 */
export function captureInviteToken(): void {
  if (!isInvitePath()) return
  const token = window.location.hash.slice(1)
  if (!token) return
  const state: InviteHistoryState = { inviteToken: token }
  window.history.replaceState(state, '', INVITE_PATH)
}

export function readInviteToken(): string | null {
  captureInviteToken()
  const state: unknown = window.history.state
  return isInviteHistoryState(state) ? state.inviteToken : null
}

export function clearInviteToken(): void {
  window.history.replaceState(null, '', INVITE_PATH)
}

import { createContext, useContext, useState, useEffect, useCallback } from 'react'
import { useSWRConfig } from 'swr'
import { getMe, login as apiLogin, logout as apiLogout, updateProfile as apiUpdateProfile, setOnUnauthorized, type AuthUser } from './api'
import { Sentry } from './sentry'

interface AuthState {
  user: AuthUser | null
  loading: boolean
  login: (email: string, password: string) => Promise<void>
  logout: () => Promise<void>
  updateProfile: (data: { name?: string; password?: string; current_password?: string }) => Promise<void>
  refreshUser: () => Promise<void>
}

const AuthContext = createContext<AuthState>({
  user: null,
  loading: true,
  login: async () => {},
  logout: async () => {},
  updateProfile: async () => {},
  refreshUser: async () => {},
})

export function useAuth() {
  return useContext(AuthContext)
}

export function AuthProvider({ children }: { children: React.ReactNode }) {
  const [user, setUser] = useState<AuthUser | null>(null)
  const [loading, setLoading] = useState(true)
  const { mutate } = useSWRConfig()

  const clearCache = useCallback(
    () => mutate(() => true, undefined, { revalidate: false }),
    [mutate],
  )

  const handleUnauthorized = useCallback(() => {
    setUser(null)
    void clearCache()
  }, [clearCache])

  useEffect(() => {
    setOnUnauthorized(handleUnauthorized)
    return () => setOnUnauthorized(null)
  }, [handleUnauthorized])

  useEffect(() => {
    getMe()
      .then((u) => {
        setUser(u)
        Sentry.setUser({ id: String(u.id), email: u.email, username: u.name })
      })
      .catch(() => {
        setUser(null)
        Sentry.setUser(null)
      })
      .finally(() => setLoading(false))
  }, [])

  const login = useCallback(async (email: string, password: string) => {
    const res = await apiLogin(email, password)
    setUser(res.user)
    Sentry.setUser({ id: String(res.user.id), email: res.user.email, username: res.user.name })
  }, [])

  const logout = useCallback(async () => {
    await apiLogout()
    setUser(null)
    Sentry.setUser(null)
    await clearCache()
  }, [clearCache])

  const refreshUser = useCallback(async () => {
    const refreshed = await getMe()
    setUser(refreshed)
  }, [])

  const updateProfile = useCallback(async (data: { name?: string; password?: string; current_password?: string }) => {
    await apiUpdateProfile(data)
    await refreshUser()
  }, [refreshUser])

  return (
    <AuthContext value={{ user, loading, login, logout, updateProfile, refreshUser }}>
      {children}
    </AuthContext>
  )
}

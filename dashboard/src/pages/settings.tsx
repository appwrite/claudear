import { useState, useRef } from 'react'
import { useAuth } from '../lib/auth'
import { uploadAvatar } from '../lib/api'
import { getInitials } from '../lib/formatters'
import { PageHeader } from '../components/layout/page-header'
import { PersonalTokensCard } from '../components/settings/personal-tokens'
import { Camera, Check, Loader2 } from 'lucide-react'

export default function SettingsPage() {
  const { user, updateProfile, refreshUser } = useAuth()

  const fileRef = useRef<HTMLInputElement>(null)
  const [avatarLoading, setAvatarLoading] = useState(false)
  const [avatarError, setAvatarError] = useState('')
  const [avatarSuccess, setAvatarSuccess] = useState(false)

  const [name, setName] = useState(user?.name ?? '')
  const [nameLoading, setNameLoading] = useState(false)
  const [nameError, setNameError] = useState('')
  const [nameSuccess, setNameSuccess] = useState(false)

  const [currentPassword, setCurrentPassword] = useState('')
  const [newPassword, setNewPassword] = useState('')
  const [confirmPassword, setConfirmPassword] = useState('')
  const [pwLoading, setPwLoading] = useState(false)
  const [pwError, setPwError] = useState('')
  const [pwSuccess, setPwSuccess] = useState(false)

  const handleAvatarChange = async (e: React.ChangeEvent<HTMLInputElement>) => {
    const file = e.target.files?.[0]
    if (!file) return
    setAvatarError('')
    setAvatarSuccess(false)
    setAvatarLoading(true)
    try {
      await uploadAvatar(file)
      await refreshUser()
      setAvatarSuccess(true)
      setTimeout(() => setAvatarSuccess(false), 2000)
    } catch {
      setAvatarError('Failed to upload avatar')
    } finally {
      setAvatarLoading(false)
      if (fileRef.current) fileRef.current.value = ''
    }
  }

  const handleNameSave = async (e: React.FormEvent) => {
    e.preventDefault()
    if (!name.trim()) return
    setNameError('')
    setNameSuccess(false)
    setNameLoading(true)
    try {
      await updateProfile({ name: name.trim() })
      setNameSuccess(true)
      setTimeout(() => setNameSuccess(false), 2000)
    } catch {
      setNameError('Failed to update name')
    } finally {
      setNameLoading(false)
    }
  }

  const handlePasswordSave = async (e: React.FormEvent) => {
    e.preventDefault()
    setPwError('')
    setPwSuccess(false)
    if (newPassword.length < 8) {
      setPwError('New password must be at least 8 characters')
      return
    }
    if (newPassword !== confirmPassword) {
      setPwError('Passwords do not match')
      return
    }
    setPwLoading(true)
    try {
      await updateProfile({ password: newPassword, current_password: currentPassword })
      setCurrentPassword('')
      setNewPassword('')
      setConfirmPassword('')
      setPwSuccess(true)
      setTimeout(() => setPwSuccess(false), 2000)
    } catch {
      setPwError('Failed to change password. Check your current password.')
    } finally {
      setPwLoading(false)
    }
  }

  if (!user) return null

  const inputClass =
    'w-full px-3 py-2 border rounded-md bg-background text-sm focus:outline-none focus:ring-2 focus:ring-primary'

  return (
    <div className="space-y-6">
      <PageHeader title="Settings" description="Manage your profile, account and access tokens" />

      <div className="border rounded-lg p-6 bg-card">
        <h3 className="font-medium mb-4">Profile Picture</h3>
        <div className="flex items-center gap-6">
          <button
            onClick={() => fileRef.current?.click()}
            className="relative group"
            disabled={avatarLoading}
          >
            {user.avatar_url ? (
              <img
                src={user.avatar_url}
                alt={user.name}
                className="h-20 w-20 rounded-full object-cover"
              />
            ) : (
              <div className="h-20 w-20 rounded-full bg-primary/10 text-primary flex items-center justify-center text-xl font-semibold">
                {getInitials(user.name)}
              </div>
            )}
            <div className="absolute inset-0 rounded-full bg-black/40 flex items-center justify-center opacity-0 group-hover:opacity-100 transition-opacity">
              {avatarLoading ? (
                <Loader2 className="h-5 w-5 text-white animate-spin" />
              ) : (
                <Camera className="h-5 w-5 text-white" />
              )}
            </div>
          </button>
          <div className="text-sm text-muted-foreground">
            <p>Click the avatar to upload a new image.</p>
            <p>Max 5MB. PNG, JPG, GIF, or WebP.</p>
            {avatarSuccess && <p className="text-green-500 mt-1 flex items-center gap-1"><Check className="h-3 w-3" /> Updated</p>}
            {avatarError && <p className="text-destructive mt-1">{avatarError}</p>}
          </div>
          <input
            ref={fileRef}
            type="file"
            accept="image/png,image/jpeg,image/gif,image/webp"
            onChange={handleAvatarChange}
            className="hidden"
          />
        </div>
      </div>

      <div className="border rounded-lg p-6 bg-card">
        <h3 className="font-medium mb-4">Display Name</h3>
        <form onSubmit={handleNameSave} className="flex items-end gap-4 max-w-md">
          <div className="flex-1 space-y-1">
            <label className="text-sm font-medium">Name</label>
            <input
              value={name}
              onChange={(e) => setName(e.target.value)}
              required
              className={inputClass}
            />
          </div>
          <button
            type="submit"
            disabled={nameLoading || name.trim() === user.name}
            className="px-4 py-2 bg-primary text-primary-foreground rounded-md text-sm font-medium hover:bg-primary/90 disabled:opacity-50"
          >
            {nameLoading ? 'Saving...' : 'Save'}
          </button>
        </form>
        {nameSuccess && <p className="text-green-500 text-sm mt-2 flex items-center gap-1"><Check className="h-3 w-3" /> Updated</p>}
        {nameError && <p className="text-destructive text-sm mt-2">{nameError}</p>}
      </div>

      <div className="border rounded-lg p-6 bg-card">
        <h3 className="font-medium mb-4">Change Password</h3>
        <form onSubmit={handlePasswordSave} className="space-y-4 max-w-md">
          <div className="space-y-1">
            <label className="text-sm font-medium">Current Password</label>
            <input
              type="password"
              value={currentPassword}
              onChange={(e) => setCurrentPassword(e.target.value)}
              required
              autoComplete="current-password"
              className={inputClass}
            />
          </div>
          <div className="space-y-1">
            <label className="text-sm font-medium">New Password</label>
            <input
              type="password"
              value={newPassword}
              onChange={(e) => setNewPassword(e.target.value)}
              required
              autoComplete="new-password"
              className={inputClass}
            />
          </div>
          <div className="space-y-1">
            <label className="text-sm font-medium">Confirm New Password</label>
            <input
              type="password"
              value={confirmPassword}
              onChange={(e) => setConfirmPassword(e.target.value)}
              required
              autoComplete="new-password"
              className={inputClass}
            />
          </div>
          {pwError && <p className="text-destructive text-sm">{pwError}</p>}
          {pwSuccess && <p className="text-green-500 text-sm flex items-center gap-1"><Check className="h-3 w-3" /> Password changed</p>}
          <button
            type="submit"
            disabled={pwLoading}
            className="px-4 py-2 bg-primary text-primary-foreground rounded-md text-sm font-medium hover:bg-primary/90 disabled:opacity-50"
          >
            {pwLoading ? 'Changing...' : 'Change Password'}
          </button>
        </form>
      </div>

      <PersonalTokensCard />
    </div>
  )
}

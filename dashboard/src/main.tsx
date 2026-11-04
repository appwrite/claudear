import React from 'react'
import ReactDOM from 'react-dom/client'
import { SWRConfig } from 'swr'
import { initSentry, Sentry } from './lib/sentry'
import { captureInviteToken } from './lib/invite-token'
import App from './App'

captureInviteToken()
initSentry()

ReactDOM.createRoot(document.getElementById('root')!, {
  onCaughtError: Sentry.reactErrorHandler(),
  onUncaughtError: Sentry.reactErrorHandler((error, errorInfo) => {
    console.error('Uncaught error', error, errorInfo.componentStack)
  }),
}).render(
  <React.StrictMode>
    <SWRConfig
      value={{
        dedupingInterval: 2_000,
        revalidateOnFocus: false,
        keepPreviousData: true,
        errorRetryCount: 2,
      }}
    >
      <App />
    </SWRConfig>
  </React.StrictMode>,
)

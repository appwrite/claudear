import type { ReactNode } from 'react'

export function AuthShell({
  title,
  subtitle,
  children,
}: {
  title: ReactNode
  subtitle: ReactNode
  children: ReactNode
}) {
  return (
    <div className="relative min-h-screen overflow-hidden bg-background">
      <div className="pointer-events-none absolute inset-0">
        <div className="absolute left-1/2 top-24 h-72 w-72 -translate-x-1/2 rounded-full bg-primary/12 blur-3xl" />
        <div className="absolute inset-x-0 top-0 h-40 bg-gradient-to-b from-primary/5 to-transparent" />
        <div className="absolute bottom-0 left-1/4 h-40 w-40 rounded-full bg-primary/5 blur-2xl" />
      </div>

      <div className="relative z-10 flex min-h-screen items-center justify-center px-6 py-10">
        <section className="w-full max-w-lg">
          <div className="relative overflow-hidden rounded-2xl border border-border/80 bg-card/95 shadow-2xl shadow-black/15 backdrop-blur">
            <div className="absolute inset-x-0 top-0 h-px bg-gradient-to-r from-transparent via-primary/70 to-transparent" />

            <div className="border-b border-border/80 bg-background/60 px-4 py-3">
              <div className="relative flex items-center justify-center">
                <div className="absolute left-0 flex items-center gap-2">
                  <span className="h-2.5 w-2.5 rounded-full bg-rose-400/70" />
                  <span className="h-2.5 w-2.5 rounded-full bg-amber-400/70" />
                  <span className="h-2.5 w-2.5 rounded-full bg-primary/80" />
                </div>
                <div className="flex items-center gap-0 font-mono text-sm text-muted-foreground">
                  claudear
                  <span className="ml-[0.1em] inline-block h-[1em] w-[0.5em] rounded-[0.05em] bg-primary" />
                </div>
              </div>
            </div>

            <div className="p-6 sm:p-8">
              <div className="relative mb-6 overflow-hidden rounded-xl border border-border/70 bg-background/70 p-4">
                <div className="pointer-events-none absolute inset-0 bg-gradient-to-br from-primary/8 via-transparent to-transparent" />
                <div className="relative min-w-0">
                  <h1 className="text-2xl font-semibold tracking-tight text-foreground">
                    {title}
                  </h1>
                  <p className="mt-1 text-sm text-muted-foreground">
                    {subtitle}
                  </p>
                </div>
              </div>

              {children}
            </div>
          </div>
        </section>
      </div>
    </div>
  )
}

export const AUTH_INPUT_CLASS =
  'w-full rounded-lg border border-input/90 bg-background/85 px-3 py-2.5 text-sm shadow-inner shadow-black/5 outline-none transition placeholder:text-muted-foreground/70 hover:border-border focus:border-primary/40 focus:ring-2 focus:ring-primary/20'

export const AUTH_LABEL_CLASS = 'text-xs font-semibold uppercase tracking-[0.12em] text-muted-foreground'

export const AUTH_ALERT_CLASS = 'rounded-lg border border-destructive/20 bg-destructive/10 px-3 py-2.5 text-sm text-destructive'

export const AUTH_BUTTON_CLASS =
  'group relative flex w-full items-center justify-center overflow-hidden rounded-lg border border-primary/25 bg-primary px-4 py-2.5 text-sm font-semibold text-primary-foreground shadow-lg shadow-primary/20 transition hover:bg-primary/90 disabled:cursor-not-allowed disabled:opacity-60'

export function AuthButtonLabel({ children }: { children: ReactNode }) {
  return (
    <>
      <span className="absolute inset-0 opacity-0 transition group-hover:opacity-100">
        <span className="absolute inset-y-0 left-[-30%] w-1/2 rotate-12 bg-white/20 blur-md" />
      </span>
      <span className="relative">{children}</span>
    </>
  )
}

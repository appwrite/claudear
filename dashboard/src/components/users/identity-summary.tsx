import type { UserIdentities } from '../../lib/api'

const CHIP_CLASS = 'inline-flex items-center px-2 py-0.5 rounded-full text-xs bg-muted text-muted-foreground'

export function IdentitySummary({ identities }: { identities: UserIdentities | undefined }) {
  if (!identities) {
    return <span className="text-xs text-muted-foreground">—</span>
  }
  const { person_key: personKey } = identities
  const { github_usernames: githubUsernames, discord_id: discordId, slack_id: slackId } = identities.identities
  if (!personKey && githubUsernames.length === 0 && !discordId && !slackId) {
    return <span className="text-xs text-muted-foreground">Not set</span>
  }
  return (
    <div className="flex flex-wrap items-center gap-1">
      {personKey && <span className="font-mono text-xs text-muted-foreground mr-1" title="Person key">{personKey}</span>}
      {githubUsernames.map((login) => (
        <span key={login} className={CHIP_CLASS} title="GitHub">@{login}</span>
      ))}
      {discordId && <span className={CHIP_CLASS} title={`Discord ${discordId}`}>Discord</span>}
      {slackId && <span className={CHIP_CLASS} title={`Slack ${slackId}`}>Slack</span>}
    </div>
  )
}

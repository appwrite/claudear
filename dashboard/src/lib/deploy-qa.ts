import type { DeployQaSettings, DeployQaTagFilterKind } from './api'

export interface TagFilterParts {
  kind: string
  value: string
}

export interface ValidationIssue {
  path: string
  message: string
}

const KIND_VALUE_DELIMITER = ':'

export function composeTagFilter(kind: DeployQaTagFilterKind, value: string): string {
  return kind.requires_value ? `${kind.kind}${KIND_VALUE_DELIMITER}${value.trim()}` : kind.kind
}

export function parseTagFilter(raw: string, kinds: DeployQaTagFilterKind[]): TagFilterParts {
  const filter = raw.trim()
  const exact = kinds.find(kind => kind.kind === filter)
  if (exact) {
    return { kind: exact.kind, value: '' }
  }
  const prefixed = kinds
    .filter(kind => kind.requires_value && filter.startsWith(`${kind.kind}${KIND_VALUE_DELIMITER}`))
    .sort((left, right) => right.kind.length - left.kind.length)[0]
  if (prefixed) {
    return { kind: prefixed.kind, value: filter.slice(prefixed.kind.length + KIND_VALUE_DELIMITER.length) }
  }
  return { kind: filter, value: '' }
}

export function findTagFilterKind(
  name: string,
  kinds: DeployQaTagFilterKind[],
): DeployQaTagFilterKind | undefined {
  return kinds.find(kind => kind.kind === name)
}

function isWholeNumber(value: number): boolean {
  return Number.isSafeInteger(value) && value >= 0
}

function isOwnerAndName(repo: string): boolean {
  const parts = repo.split('/')
  return parts.length === 2 && parts.every(part => part.trim() !== '')
}

export function validateDeployQaSettings(
  settings: DeployQaSettings,
  kinds: DeployQaTagFilterKind[],
  separator: string,
): ValidationIssue[] {
  const issues: ValidationIssue[] = []

  if (!isWholeNumber(settings.poll_interval_ms)) {
    issues.push({ path: 'poll_interval_ms', message: 'Poll interval must be a whole number of milliseconds.' })
  }
  if (!isWholeNumber(settings.timeout_secs)) {
    issues.push({ path: 'timeout_secs', message: 'Timeout must be a whole number of seconds.' })
  }

  const seenNames = new Set<string>()
  settings.tracks.forEach((track, index) => {
    const name = track.name
    if (name.trim() === '') {
      issues.push({ path: `tracks[${index}].name`, message: 'Track name is required.' })
    } else if (name !== name.trim()) {
      issues.push({ path: `tracks[${index}].name`, message: 'Track name must not start or end with whitespace.' })
    } else if (separator !== '' && name.includes(separator)) {
      issues.push({ path: `tracks[${index}].name`, message: `Track name must not contain '${separator}'.` })
    } else if (seenNames.has(name)) {
      issues.push({ path: `tracks[${index}].name`, message: `Track name '${name}' is used by more than one track.` })
    }
    seenNames.add(name)

    if (!isOwnerAndName(track.repo)) {
      issues.push({ path: `tracks[${index}].repo`, message: 'Repository must be in owner/name form.' })
    }

    const filter = parseTagFilter(track.tag_filter, kinds)
    const kind = findTagFilterKind(filter.kind, kinds)
    if (!kind) {
      issues.push({ path: `tracks[${index}].tag_filter`, message: `Unknown tag filter kind '${filter.kind}'.` })
    } else if (kind.requires_value && filter.value.trim() === '') {
      issues.push({ path: `tracks[${index}].tag_filter`, message: `Tag filter '${kind.kind}' requires a value.` })
    }
  })

  return issues
}

export function isExternalUrl(url: string | null): url is string {
  return url !== null && /^https?:\/\//i.test(url)
}

export function formatDuration(seconds: number): string {
  if (seconds < 60) return `${seconds}s`
  if (seconds < 3600) return `${Math.round(seconds / 6) / 10}m`
  return `${Math.round(seconds / 360) / 10}h`
}

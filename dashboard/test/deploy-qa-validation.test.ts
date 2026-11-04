import { describe, expect, test } from 'bun:test'
import type { DeployQaSettings, DeployQaTagFilterKind, DeployQaTrackSettings } from '../src/lib/api'
import {
  composeTagFilter,
  formatDuration,
  isExternalUrl,
  parseTagFilter,
  validateDeployQaSettings,
} from '../src/lib/deploy-qa'

const kinds: DeployQaTagFilterKind[] = [
  { kind: 'any', requires_value: false },
  { kind: 'suffix', requires_value: true },
  { kind: 'not_suffix', requires_value: true },
]
const futureKinds: DeployQaTagFilterKind[] = [...kinds, { kind: 'prefix', requires_value: true }]
const separator = ':'

function settingsWith(tracks: DeployQaTrackSettings[], overrides: Partial<DeployQaSettings> = {}): DeployQaSettings {
  return {
    enabled: true,
    poll_interval_ms: 300000,
    skip_if_previous_running: true,
    timeout_secs: 1800,
    discord_channel_id: null,
    github_discord_map_path: null,
    instructions_path: null,
    tracks,
    ...overrides,
  }
}

function paths(settings: DeployQaSettings, available = kinds): string[] {
  return validateDeployQaSettings(settings, available, separator).map(issue => issue.path)
}

describe('composeTagFilter', () => {
  test('a kind without a value composes to the kind name alone', () => {
    expect(composeTagFilter(kinds[0], 'ignored')).toBe('any')
  })

  test('a kind with a value composes to kind:value with the value trimmed', () => {
    expect(composeTagFilter(kinds[1], '  -db ')).toBe('suffix:-db')
    expect(composeTagFilter(kinds[2], '-rc')).toBe('not_suffix:-rc')
  })

  test('an unknown future kind composes from the API description alone', () => {
    expect(composeTagFilter({ kind: 'prefix', requires_value: true }, 'v2.')).toBe('prefix:v2.')
  })
})

describe('parseTagFilter', () => {
  test('parses kinds without and with values', () => {
    expect(parseTagFilter('any', kinds)).toEqual({ kind: 'any', value: '' })
    expect(parseTagFilter('suffix:-db', kinds)).toEqual({ kind: 'suffix', value: '-db' })
    expect(parseTagFilter('not_suffix:-db', kinds)).toEqual({ kind: 'not_suffix', value: '-db' })
  })

  test('keeps colons inside the value', () => {
    expect(parseTagFilter('suffix:a:b', kinds)).toEqual({ kind: 'suffix', value: 'a:b' })
  })

  test('a value kind written without its value parses to that kind with an empty value', () => {
    expect(parseTagFilter('suffix', kinds)).toEqual({ kind: 'suffix', value: '' })
    expect(parseTagFilter('suffix:', kinds)).toEqual({ kind: 'suffix', value: '' })
  })

  test('an unrecognised filter is returned whole as its kind', () => {
    expect(parseTagFilter('prefix:v2.', kinds)).toEqual({ kind: 'prefix:v2.', value: '' })
  })

  test('a future kind listed by the API parses without any code change', () => {
    expect(parseTagFilter('prefix:v2.', futureKinds)).toEqual({ kind: 'prefix', value: 'v2.' })
    const parsed = parseTagFilter('prefix:v2.', futureKinds)
    expect(composeTagFilter(futureKinds[3], parsed.value)).toBe('prefix:v2.')
  })
})

describe('validateDeployQaSettings', () => {
  const valid: DeployQaTrackSettings[] = [
    { name: 'cloud', repo: 'appwrite/cloud', tag_filter: 'any' },
    { name: 'edge-db', repo: 'appwrite/edge', tag_filter: 'suffix:-db' },
  ]

  test('accepts well-formed settings', () => {
    expect(validateDeployQaSettings(settingsWith(valid), kinds, separator)).toEqual([])
    expect(validateDeployQaSettings(settingsWith([]), kinds, separator)).toEqual([])
  })

  test('rejects an unknown kind', () => {
    const issues = validateDeployQaSettings(
      settingsWith([{ name: 'cloud', repo: 'appwrite/cloud', tag_filter: 'prefix:v2.' }]),
      kinds,
      separator,
    )
    expect(issues).toEqual([{ path: 'tracks[0].tag_filter', message: "Unknown tag filter kind 'prefix:v2.'." }])
  })

  test('accepts a future kind once the API lists it', () => {
    const settings = settingsWith([{ name: 'cloud', repo: 'appwrite/cloud', tag_filter: 'prefix:v2.' }])
    expect(validateDeployQaSettings(settings, futureKinds, separator)).toEqual([])
  })

  test('requires a value for kinds that need one, including future kinds', () => {
    expect(paths(settingsWith([{ name: 'cloud', repo: 'appwrite/cloud', tag_filter: 'suffix:' }]))).toEqual([
      'tracks[0].tag_filter',
    ])
    expect(paths(settingsWith([{ name: 'cloud', repo: 'appwrite/cloud', tag_filter: 'not_suffix:  ' }]))).toEqual([
      'tracks[0].tag_filter',
    ])
    const issues = validateDeployQaSettings(
      settingsWith([{ name: 'cloud', repo: 'appwrite/cloud', tag_filter: 'prefix:' }]),
      futureKinds,
      separator,
    )
    expect(issues).toEqual([{ path: 'tracks[0].tag_filter', message: "Tag filter 'prefix' requires a value." }])
  })

  test('requires a non-empty trimmed name', () => {
    const issues = validateDeployQaSettings(
      settingsWith([{ name: '   ', repo: 'appwrite/cloud', tag_filter: 'any' }]),
      kinds,
      separator,
    )
    expect(issues).toEqual([{ path: 'tracks[0].name', message: 'Track name is required.' }])
  })

  test('rejects a name with leading or trailing whitespace', () => {
    for (const name of [' cloud', 'cloud ', '\tcloud', 'cloud\n']) {
      expect(validateDeployQaSettings(settingsWith([{ name, repo: 'appwrite/cloud', tag_filter: 'any' }]), kinds, separator))
        .toEqual([{ path: 'tracks[0].name', message: 'Track name must not start or end with whitespace.' }])
    }
  })

  test('allows inner whitespace in a name', () => {
    expect(
      validateDeployQaSettings(settingsWith([{ name: 'cloud main', repo: 'appwrite/cloud', tag_filter: 'any' }]), kinds, separator),
    ).toEqual([])
  })

  test('rejects a name containing the separator from the API', () => {
    const settings = settingsWith([{ name: 'cloud:main', repo: 'appwrite/cloud', tag_filter: 'any' }])
    expect(validateDeployQaSettings(settings, kinds, separator)).toEqual([
      { path: 'tracks[0].name', message: "Track name must not contain ':'." },
    ])
    expect(validateDeployQaSettings(settings, kinds, '|')).toEqual([])
  })

  test('rejects duplicate names on the later track', () => {
    const issues = validateDeployQaSettings(
      settingsWith([...valid, { name: 'cloud', repo: 'appwrite/other', tag_filter: 'any' }]),
      kinds,
      separator,
    )
    expect(issues).toEqual([{ path: 'tracks[2].name', message: "Track name 'cloud' is used by more than one track." }])
  })

  test('requires the repo to be owner/name', () => {
    for (const repo of ['appwrite', 'appwrite/cloud/extra', '/cloud', 'appwrite/', ' /cloud', '']) {
      expect(paths(settingsWith([{ name: 'cloud', repo, tag_filter: 'any' }]))).toEqual(['tracks[0].repo'])
    }
  })

  test('requires whole, non-negative numbers for the poll interval and timeout', () => {
    expect(paths(settingsWith(valid, { poll_interval_ms: Number.NaN }))).toEqual(['poll_interval_ms'])
    expect(paths(settingsWith(valid, { poll_interval_ms: 1.5 }))).toEqual(['poll_interval_ms'])
    expect(paths(settingsWith(valid, { timeout_secs: -1 }))).toEqual(['timeout_secs'])
    expect(paths(settingsWith(valid, { poll_interval_ms: 0, timeout_secs: 0 }))).toEqual([])
  })

  test('reports every problem with its path', () => {
    const issues = paths(
      settingsWith([
        { name: '', repo: 'nope', tag_filter: 'suffix:' },
        { name: 'a:b', repo: 'a/b', tag_filter: 'mystery' },
      ]),
    )
    expect(issues).toEqual([
      'tracks[0].name',
      'tracks[0].repo',
      'tracks[0].tag_filter',
      'tracks[1].name',
      'tracks[1].tag_filter',
    ])
  })
})

describe('helpers', () => {
  test('isExternalUrl accepts only http and https URLs', () => {
    expect(isExternalUrl('https://github.com/appwrite/cloud/releases/tag/1.0.0')).toBe(true)
    expect(isExternalUrl('http://example.com')).toBe(true)
    expect(isExternalUrl('javascript:alert(1)')).toBe(false)
    expect(isExternalUrl(null)).toBe(false)
  })

  test('formatDuration picks a readable unit', () => {
    expect(formatDuration(45)).toBe('45s')
    expect(formatDuration(300)).toBe('5m')
    expect(formatDuration(1800)).toBe('30m')
    expect(formatDuration(5400)).toBe('1.5h')
  })
})

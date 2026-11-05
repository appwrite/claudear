import { useState, useEffect, useCallback, useMemo } from 'react'
import useSWR from 'swr'
import {
  ApiError,
  fetchConfig, saveConfig, type ConfigErrorCode, type ConfigResponse,
  fetchGlobalInstruction, saveGlobalInstruction, type InstructionResponse,
} from '../lib/api'
import { PageHeader } from '../components/layout/page-header'
import { CardStackSkeleton } from '../components/shared/page-skeletons'
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from '../components/ui/card'
import { Badge } from '../components/ui/badge'
import {
  Settings, Save, RotateCcw, FileText, AlertTriangle,
  CheckCircle2, Server, GitBranch, Bell, Shield,
  Cpu, Bot, RefreshCw, Eye, X, ShieldAlert, Rocket,
  Database, MessageSquare, Lock, Search, Users, Brain,
  Gauge, Sparkles, BookOpen, Info, Plus, ListChecks, Cloud, Boxes, Plug, Layers,
} from 'lucide-react'
import { useAuth } from '../lib/auth'
import { useRouter } from '../router'

type SectionKind = 'root' | 'table' | 'array'

type FieldType = 'string' | 'multiline' | 'number' | 'boolean' | 'array' | 'raw'

interface ParsedField {
  key: string
  rawKey: string
  path: string
  value: string
  type: FieldType
  comment?: string
  start: number
  end: number
  indent: string
}

interface TomlSection {
  key: string
  kind: SectionKind
  index: number
  path: string
  header: string
  lines: string[]
  fields: ParsedField[]
}

interface SectionMeta {
  label: string
  icon: React.ReactNode
  description: string
}

interface SaveResult {
  ok: boolean
  message: string
}

const ROOT_KEY = '_top'
const ADMIN_ROLE = 'admin'
const REDACTED = '[REDACTED]'
const REVISION_MISMATCH: ConfigErrorCode = 'revision_mismatch'
const DEPLOY_QA_KEY = 'deploy_qa'
const DEPLOY_QA_ROUTE = '/deploy-qa'
const WILDCARD_SUFFIX = '.*'

const SECTION_META: Record<string, SectionMeta> = {
  [ROOT_KEY]: { label: 'Core', icon: <Server className="h-4 w-4 text-primary" />, description: 'Workspace, orgs, discovery paths, polling and HTTP server' },
  agent: { label: 'Agent', icon: <Bot className="h-4 w-4 text-violet-500" />, description: 'Default provider, timeouts, experiments and orchestration' },
  'agent.providers.*': { label: 'Agent Provider', icon: <Bot className="h-4 w-4 text-violet-400" />, description: 'Provider command, model, environment and MCP servers' },
  'scm.github': { label: 'GitHub', icon: <GitBranch className="h-4 w-4 text-gray-700" />, description: 'Token, PR monitoring, and webhooks' },
  'scm.github.app': { label: 'GitHub App', icon: <Shield className="h-4 w-4 text-gray-500" />, description: 'App-based authentication config' },
  'scm.gitlab': { label: 'GitLab', icon: <GitBranch className="h-4 w-4 text-orange-600" />, description: 'GitLab merge request integration' },
  'issues.*': { label: 'Issue Source', icon: <AlertTriangle className="h-4 w-4 text-red-500" />, description: 'Issue source triggers and filters' },
  'notifiers.*': { label: 'Notifier', icon: <Bell className="h-4 w-4 text-indigo-500" />, description: 'Notification channel settings' },
  ask: { label: 'Human Q&A', icon: <Eye className="h-4 w-4 text-cyan-500" />, description: 'Ask-loop timeouts and semantic reuse' },
  retry: { label: 'Retry Strategy', icon: <RefreshCw className="h-4 w-4 text-orange-500" />, description: 'Retry timing and limits for failed fixes' },
  regression: { label: 'Regression', icon: <Shield className="h-4 w-4 text-red-400" />, description: 'Post-fix regression monitoring' },
  [DEPLOY_QA_KEY]: { label: 'Deploy QA', icon: <Rocket className="h-4 w-4 text-sky-500" />, description: 'Live QA of new GitHub release tips' },
  'deploy_qa.tracks': { label: 'Deploy QA Track', icon: <Rocket className="h-4 w-4 text-sky-400" />, description: 'Repository and tag filter watched for new releases' },
  cascade: { label: 'Cascade', icon: <RefreshCw className="h-4 w-4 text-teal-500" />, description: 'Multi-repo cascade chaining' },
  learning: { label: 'Learning', icon: <Cpu className="h-4 w-4 text-emerald-500" />, description: 'Continuous learning and knowledge extraction' },
  prioritisation: { label: 'Prioritisation', icon: <ListChecks className="h-4 w-4 text-amber-500" />, description: 'Issue prioritisation engine' },
  code_index: { label: 'Code Index', icon: <Database className="h-4 w-4 text-blue-500" />, description: 'Repository code indexing' },
  retrieval_eval: { label: 'Retrieval Evaluation', icon: <Search className="h-4 w-4 text-blue-400" />, description: 'Retrieval-quality assessment' },
  evaluation: { label: 'Self-Evaluation', icon: <Gauge className="h-4 w-4 text-lime-600" />, description: 'Self-evaluation of agent runs' },
  dashboard: { label: 'Dashboard', icon: <Layers className="h-4 w-4 text-primary" />, description: 'Dashboard display settings' },
  llm: { label: 'Local LLM', icon: <Brain className="h-4 w-4 text-fuchsia-500" />, description: 'Model path, download URL, and hardware settings' },
  chat: { label: 'Code Chat', icon: <MessageSquare className="h-4 w-4 text-fuchsia-400" />, description: 'Local code chat' },
  tls: { label: 'TLS', icon: <Lock className="h-4 w-4 text-green-600" />, description: "Let's Encrypt certificate provisioning" },
  embedding: { label: 'Embedding', icon: <Boxes className="h-4 w-4 text-purple-500" />, description: 'Embedding model acceleration and pool size' },
  qa: { label: 'Question Answering', icon: <Sparkles className="h-4 w-4 text-yellow-500" />, description: 'RAG-grounded question answering' },
  'knowledgebase.discord': { label: 'Discord Knowledgebase', icon: <BookOpen className="h-4 w-4 text-indigo-400" />, description: 'Discord channel indexing for search' },
  reports: { label: 'Reports', icon: <FileText className="h-4 w-4 text-slate-500" />, description: 'Scheduled reports and digests' },
  'reports.*': { label: 'Report', icon: <FileText className="h-4 w-4 text-slate-400" />, description: 'Scheduled report settings' },
  mcp_server: { label: 'MCP Server', icon: <Plug className="h-4 w-4 text-cyan-600" />, description: 'Built-in MCP search server' },
  'mcp_server.cf_access': { label: 'Cloudflare Access', icon: <Cloud className="h-4 w-4 text-orange-400" />, description: 'Cloudflare Access verification for the MCP server' },
  'users.*': { label: 'User', icon: <Users className="h-4 w-4 text-gray-500" />, description: 'User slug mapping to source and notification IDs' },
}

const FALLBACK_ICON = <Settings className="h-4 w-4 text-gray-400" />

function sectionMeta(key: string): SectionMeta {
  const exact = SECTION_META[key]
  if (exact) return exact

  let match: { prefix: string; meta: SectionMeta } | null = null
  for (const [pattern, meta] of Object.entries(SECTION_META)) {
    if (!pattern.endsWith(WILDCARD_SUFFIX)) continue
    const prefix = pattern.slice(0, -1)
    if (key.startsWith(prefix) && (!match || prefix.length > match.prefix.length)) {
      match = { prefix, meta }
    }
  }
  if (match) {
    return { ...match.meta, label: `${match.meta.label}: ${key.slice(match.prefix.length)}` }
  }
  return { label: key, icon: FALLBACK_ICON, description: '' }
}

function sectionLabel(section: TomlSection): string {
  const { label } = sectionMeta(section.key)
  return section.kind === 'array' ? `${label} #${section.index + 1}` : label
}

function splitDottedKey(raw: string): string[] {
  const segments: string[] = []
  let current = ''
  let quote: string | null = null
  for (const character of raw) {
    if (quote) {
      if (character === quote) quote = null
      else current += character
    } else if (character === '"' || character === "'") {
      quote = character
    } else if (character === '.') {
      segments.push(current.trim())
      current = ''
    } else {
      current += character
    }
  }
  segments.push(current.trim())
  return segments
}

function normaliseKey(raw: string): string {
  return splitDottedKey(raw).join('.')
}

function closingTripleQuote(text: string, from: number, delimiter: string): number {
  const escapes = delimiter === '"""'
  let position = from
  while (position < text.length) {
    if (escapes && text[position] === '\\') {
      position += 2
      continue
    }
    if (text.startsWith(delimiter, position)) {
      let end = position + 3
      while (end < text.length && text[end] === delimiter[0] && end - position < 5) end++
      return end
    }
    position++
  }
  return -1
}

function closingQuote(text: string, from: number, quote: string): number {
  const escapes = quote === '"'
  let position = from
  while (position < text.length && text[position] !== '\n') {
    if (escapes && text[position] === '\\') {
      position += 2
      continue
    }
    if (text[position] === quote) return position + 1
    position++
  }
  return -1
}

interface ValueScan {
  complete: boolean
  commentStart: number
}

function scanValue(text: string): ValueScan {
  let depth = 0
  let position = 0
  let commentStart = -1
  while (position < text.length) {
    const character = text[position]
    if (text.startsWith('"""', position) || text.startsWith("'''", position)) {
      const end = closingTripleQuote(text, position + 3, text.slice(position, position + 3))
      if (end < 0) return { complete: false, commentStart }
      position = end
      continue
    }
    if (character === '"' || character === "'") {
      const end = closingQuote(text, position + 1, character)
      if (end < 0) return { complete: true, commentStart }
      position = end
      continue
    }
    if (character === '#') {
      if (depth === 0 && commentStart < 0) commentStart = position
      const lineEnd = text.indexOf('\n', position)
      if (lineEnd < 0) break
      position = lineEnd
      continue
    }
    if (character === '[' || character === '{') depth++
    if (character === ']' || character === '}') depth--
    position++
  }
  return { complete: depth <= 0, commentStart }
}

function stripTrailingComment(text: string): string {
  const { commentStart } = scanValue(text)
  return (commentStart >= 0 ? text.slice(0, commentStart) : text).trim()
}

function decodeString(raw: string): string {
  if (raw.startsWith("'")) return raw.slice(1, -1)
  try {
    const decoded: unknown = JSON.parse(raw)
    return typeof decoded === 'string' ? decoded : raw.slice(1, -1)
  } catch {
    return raw.slice(1, -1)
  }
}

function encodeString(value: string): string {
  return JSON.stringify(value)
}

function parseStringArray(value: string): string[] | null {
  if (!value.startsWith('[') || !value.endsWith(']')) return null
  const inner = value.slice(1, -1)
  const items: string[] = []
  let position = 0
  while (position < inner.length) {
    const character = inner[position]
    if (/[\s,]/.test(character)) {
      position++
      continue
    }
    if (character === '#') {
      const lineEnd = inner.indexOf('\n', position)
      position = lineEnd < 0 ? inner.length : lineEnd
      continue
    }
    if (character !== '"' && character !== "'") return null
    if (inner.startsWith(character.repeat(3), position)) return null
    const end = closingQuote(inner, position + 1, character)
    if (end < 0) return null
    items.push(decodeString(inner.slice(position, end)))
    position = end
  }
  return items
}

function serializeStringArray(items: string[]): string {
  return `[${items.map(encodeString).join(', ')}]`
}

const NUMBER_PATTERN = /^[+-]?(?:\d[\d_]*(?:\.\d[\d_]*)?(?:[eE][+-]?\d+)?|inf|nan)$/

function fieldType(value: string): FieldType {
  if (value.startsWith('"""') || value.startsWith("'''")) return 'multiline'
  if (value === 'true' || value === 'false') return 'boolean'
  if (value.startsWith('"') || value.startsWith("'")) return 'string'
  if (NUMBER_PATTERN.test(value)) return 'number'
  if (value.startsWith('[') && parseStringArray(value) !== null) return 'array'
  return 'raw'
}

const ARRAY_HEADER = /^\s*\[\[\s*(.+?)\s*\]\]\s*(?:#.*)?$/
const TABLE_HEADER = /^\s*\[\s*([^[\]]+?)\s*\]\s*(?:#.*)?$/
const KEY_VALUE = /^(\s*)((?:"[^"]*"|'[^']*'|[A-Za-z0-9_\-.\s])+?)\s*=\s*(.*)$/

function joinPath(prefix: string, key: string): string {
  return prefix ? `${prefix}.${key}` : key
}

function parseDocument(content: string): TomlSection[] {
  const lines = content.split('\n')
  const sections: TomlSection[] = []
  const arrayCounts = new Map<string, number>()
  let latestArray: { key: string; index: number } | null = null
  let current: TomlSection = { key: ROOT_KEY, kind: 'root', index: 0, path: '', header: 'root', lines: [], fields: [] }
  let pendingComment = ''

  const pushCurrent = () => {
    if (current.lines.length > 0) sections.push(current)
  }

  for (let lineIndex = 0; lineIndex < lines.length; lineIndex++) {
    const line = lines[lineIndex]
    const trimmed = line.trim()

    const arrayHeader = line.match(ARRAY_HEADER)
    const tableHeader = arrayHeader ? null : line.match(TABLE_HEADER)
    if (arrayHeader || tableHeader) {
      pushCurrent()
      pendingComment = ''
      const key = normaliseKey((arrayHeader ?? tableHeader)![1])
      if (arrayHeader) {
        const index = arrayCounts.get(key) ?? 0
        arrayCounts.set(key, index + 1)
        latestArray = { key, index }
        current = { key, kind: 'array', index, path: `${key}[${index}]`, header: `[[${key}]]`, lines: [line], fields: [] }
      } else {
        const path = latestArray && key.startsWith(`${latestArray.key}.`)
          ? `${latestArray.key}[${latestArray.index}]${key.slice(latestArray.key.length)}`
          : key
        current = { key, kind: 'table', index: 0, path, header: `[${key}]`, lines: [line], fields: [] }
      }
      continue
    }

    if (trimmed === '' || trimmed.startsWith('# ==')) {
      pendingComment = ''
      current.lines.push(line)
      continue
    }
    if (trimmed.startsWith('#')) {
      pendingComment = trimmed.replace(/^#\s*/, '')
      current.lines.push(line)
      continue
    }

    const keyValue = line.match(KEY_VALUE)
    if (!keyValue) {
      current.lines.push(line)
      continue
    }

    const start = current.lines.length
    current.lines.push(line)
    let valueText = keyValue[3]
    while (!scanValue(valueText).complete && lineIndex + 1 < lines.length) {
      lineIndex++
      current.lines.push(lines[lineIndex])
      valueText += `\n${lines[lineIndex]}`
    }

    const value = stripTrailingComment(valueText)
    const rawKey = keyValue[2].trim()
    const key = normaliseKey(rawKey)
    current.fields.push({
      key,
      rawKey,
      path: joinPath(current.path, key),
      value,
      type: fieldType(value),
      comment: pendingComment || undefined,
      start,
      end: current.lines.length - 1,
      indent: keyValue[1],
    })
    pendingComment = ''
  }
  pushCurrent()
  return sections
}

function serializeDocument(sections: TomlSection[]): string {
  return sections.map(section => section.lines.join('\n')).join('\n')
}

function buildFieldMap(sections: TomlSection[]): Map<string, string> {
  const map = new Map<string, string>()
  for (const section of sections) {
    for (const field of section.fields) map.set(field.path, field.value)
  }
  return map
}

function replaceField(content: string, sectionIndex: number, path: string, value: string): string {
  const sections = parseDocument(content)
  const section = sections[sectionIndex]
  const field = section?.fields.find(candidate => candidate.path === path)
  if (!section || !field) return content
  section.lines = [
    ...section.lines.slice(0, field.start),
    `${field.indent}${field.rawKey} = ${value}`,
    ...section.lines.slice(field.end + 1),
  ]
  return serializeDocument(sections)
}

function appendTemplate(content: string, template: string): string {
  const base = content.replace(/\s+$/, '')
  return `${base}${base ? '\n\n' : ''}${template.trim()}\n`
}

type FieldChangeHandler = (field: ParsedField, value: string) => void

function containsSecret(path: string, secretPaths: Set<string>): boolean {
  if (secretPaths.has(path)) return true
  for (const secretPath of secretPaths) {
    if (secretPath.startsWith(`${path}[`) || secretPath.startsWith(`${path}.`)) return true
  }
  return false
}

const INPUT_CLASS = 'w-full px-3 py-1.5 text-sm border rounded-md bg-background focus:outline-none focus:ring-2 focus:ring-primary/30'

function ArrayFieldInput({
  field,
  onChange,
  changed,
}: {
  field: ParsedField
  onChange: FieldChangeHandler
  changed?: boolean
}) {
  const [newItem, setNewItem] = useState('')
  const items = parseStringArray(field.value) ?? []

  const addItem = () => {
    if (!newItem.trim()) return
    onChange(field, serializeStringArray([...items, newItem.trim()]))
    setNewItem('')
  }

  return (
    <div className={`space-y-2 ${changed ? 'rounded-md ring-2 ring-amber-400/50 p-1.5 -m-1.5' : ''}`}>
      <div className="flex flex-wrap gap-1">
        {items.length === 0 && (
          <span className="text-xs text-muted-foreground italic">Empty</span>
        )}
        {items.map((item, itemIndex) => (
          <span
            key={itemIndex}
            className="inline-flex items-center gap-1 px-2 py-0.5 rounded-md text-xs bg-muted border"
          >
            {item}
            <button
              type="button"
              aria-label={`Remove ${item}`}
              onClick={() => onChange(field, serializeStringArray(items.filter((_, other) => other !== itemIndex)))}
              className="text-muted-foreground hover:text-foreground"
            >
              <X className="h-3 w-3" />
            </button>
          </span>
        ))}
      </div>
      <div className="flex gap-1">
        <input
          type="text"
          aria-label={field.path}
          value={newItem}
          onChange={e => setNewItem(e.target.value)}
          onKeyDown={e => {
            if (e.key === 'Enter') {
              e.preventDefault()
              addItem()
            }
          }}
          placeholder="Add item..."
          className="flex-1 px-2 py-1 text-xs border rounded-md bg-background focus:outline-none focus:ring-2 focus:ring-primary/30"
        />
        <button
          type="button"
          onClick={addItem}
          className="px-2 py-1 text-xs border rounded-md hover:bg-muted"
        >
          Add
        </button>
      </div>
    </div>
  )
}

function SecretFieldInput({
  field,
  savedValue,
  onChange,
  changed,
}: {
  field: ParsedField
  savedValue: string | undefined
  onChange: FieldChangeHandler
  changed?: boolean
}) {
  const original = savedValue ?? '""'
  const isSet = decodeString(original) === REDACTED
  const edited = field.value !== original
  return (
    <input
      type="password"
      aria-label={field.path}
      autoComplete="new-password"
      value={edited ? decodeString(field.value) : ''}
      placeholder={isSet ? 'Set (hidden), type to replace' : 'Not set'}
      onChange={e => onChange(field, e.target.value === '' ? original : encodeString(e.target.value))}
      className={`${INPUT_CLASS} ${changed ? 'ring-2 ring-amber-400/50' : ''}`}
    />
  )
}

function ReadOnlyField({
  field,
  isSecret,
  onEditRaw,
}: {
  field: ParsedField
  isSecret: boolean
  onEditRaw: () => void
}) {
  return (
    <div className="space-y-1">
      {isSecret ? (
        <p className="text-xs text-muted-foreground italic">Secret value hidden</p>
      ) : (
        <pre className="text-xs font-mono bg-muted/50 border rounded-md px-3 py-1.5 max-h-32 overflow-auto whitespace-pre-wrap break-all">
          {field.value}
        </pre>
      )}
      <button
        type="button"
        onClick={onEditRaw}
        className="text-[11px] text-primary hover:underline"
      >
        Edit in Raw view
      </button>
    </div>
  )
}

function FieldInput({
  field,
  savedValue,
  isSecret,
  onChange,
  onEditRaw,
  changed,
}: {
  field: ParsedField
  savedValue: string | undefined
  isSecret: boolean
  onChange: FieldChangeHandler
  onEditRaw: () => void
  changed?: boolean
}) {
  const changedRing = changed ? 'ring-2 ring-amber-400/50' : ''

  if (isSecret && field.type === 'string') {
    return <SecretFieldInput field={field} savedValue={savedValue} onChange={onChange} changed={changed} />
  }

  if (isSecret || field.type === 'multiline' || field.type === 'raw') {
    return <ReadOnlyField field={field} isSecret={isSecret} onEditRaw={onEditRaw} />
  }

  if (field.type === 'boolean') {
    const checked = field.value === 'true'
    return (
      <button
        type="button"
        role="switch"
        aria-checked={checked}
        aria-label={field.path}
        onClick={() => onChange(field, checked ? 'false' : 'true')}
        className={`relative w-10 h-5 rounded-full transition-colors cursor-pointer ${
          checked ? 'bg-primary' : 'bg-muted-foreground/30'
        } ${changedRing}`}
      >
        <span
          className={`absolute top-0.5 left-0.5 w-4 h-4 rounded-full bg-white transition-transform ${
            checked ? 'translate-x-5' : ''
          }`}
        />
      </button>
    )
  }

  if (field.type === 'number') {
    return (
      <input
        type="number"
        aria-label={field.path}
        value={field.value}
        onChange={e => onChange(field, e.target.value)}
        className={`${INPUT_CLASS} ${changedRing}`}
      />
    )
  }

  if (field.type === 'array') {
    return <ArrayFieldInput field={field} onChange={onChange} changed={changed} />
  }

  return (
    <input
      type="text"
      aria-label={field.path}
      value={decodeString(field.value)}
      onChange={e => onChange(field, encodeString(e.target.value))}
      className={`${INPUT_CLASS} ${changedRing}`}
    />
  )
}

function SectionFormCard({
  section,
  savedFields,
  secretPaths,
  runtimeOverrides,
  onChange,
  onEditRaw,
}: {
  section: TomlSection
  savedFields: Map<string, string>
  secretPaths: Set<string>
  runtimeOverrides: Set<string>
  onChange: FieldChangeHandler
  onEditRaw: () => void
}) {
  const { navigate } = useRouter()
  const meta = sectionMeta(section.key)
  const isChanged = (field: ParsedField) => {
    const savedValue = savedFields.get(field.path)
    return savedValue !== undefined && savedValue !== field.value
  }
  const hasChanges = section.fields.some(isChanged)

  return (
    <Card className={hasChanges ? 'ring-2 ring-amber-400/40' : ''} data-section={section.path || ROOT_KEY}>
      <CardHeader className="pb-3">
        <div className="flex items-center justify-between gap-2">
          <div className="flex items-center gap-2">
            {meta.icon}
            <CardTitle className="text-base">{sectionLabel(section)}</CardTitle>
            {hasChanges && (
              <Badge variant="outline" className="text-[10px] text-amber-600 border-amber-300 bg-amber-50">
                Modified
              </Badge>
            )}
            {runtimeOverrides.has(section.path) && (
              <Badge variant="outline" className="text-[10px] text-sky-700 border-sky-300 bg-sky-50">
                overridden at runtime
              </Badge>
            )}
          </div>
          <Badge variant="outline" className="text-xs font-mono">
            {section.header}
          </Badge>
        </div>
        {meta.description && (
          <CardDescription className="text-xs">{meta.description}</CardDescription>
        )}
        {section.key === DEPLOY_QA_KEY && (
          <button
            type="button"
            onClick={() => navigate(DEPLOY_QA_ROUTE)}
            className="self-start text-xs text-primary hover:underline flex items-center gap-1"
          >
            <Rocket className="h-3.5 w-3.5" /> Open Deploy QA
          </button>
        )}
      </CardHeader>
      <CardContent>
        {section.fields.length > 0 ? (
          <div className="space-y-3">
            {section.fields.map(field => {
              const isSecret = containsSecret(field.path, secretPaths)
              return (
                <div key={field.path}>
                  <div className="flex items-center gap-2 mb-1 flex-wrap">
                    <label className="text-xs font-medium font-mono">{field.key}</label>
                    {isSecret && (
                      <span className="text-[10px] text-muted-foreground bg-muted px-1.5 py-0.5 rounded">secret</span>
                    )}
                    {runtimeOverrides.has(field.path) && (
                      <span
                        className="text-[10px] text-sky-700 bg-sky-50 border border-sky-200 px-1.5 py-0.5 rounded"
                        title="Overridden at runtime (environment variable or instructions_file); the running value differs"
                      >
                        overridden at runtime (environment variable or instructions_file); the running value differs
                      </span>
                    )}
                  </div>
                  {field.comment && (
                    <p className="text-[11px] text-muted-foreground mb-1">{field.comment}</p>
                  )}
                  <FieldInput
                    field={field}
                    savedValue={savedFields.get(field.path)}
                    isSecret={isSecret}
                    onChange={onChange}
                    onEditRaw={onEditRaw}
                    changed={isChanged(field)}
                  />
                </div>
              )
            })}
          </div>
        ) : (
          <p className="text-xs text-muted-foreground italic">Empty section</p>
        )}
      </CardContent>
    </Card>
  )
}

function GlobalInstructionsCard() {
  const { data, error, isLoading, mutate } = useSWR<InstructionResponse>('global-instruction', fetchGlobalInstruction)
  const [draft, setDraft] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)
  const [saved, setSaved] = useState(false)
  const [saveError, setSaveError] = useState<string | null>(null)

  const serverText = data?.text ?? ''
  const text = draft ?? serverText
  const dirty = text !== serverText

  const handleSave = useCallback(async () => {
    const saved = text
    setSaving(true)
    setSaveError(null)
    try {
      await saveGlobalInstruction(saved)
    } catch (e: any) {
      setSaveError(e?.message || 'Failed to save instructions')
      return
    } finally {
      setSaving(false)
    }
    // PUT persisted. Sync the cache optimistically from the known-saved value so
    // a background revalidation failure can't misreport the save or blank the
    // editor. Only clear the draft if the user has not typed more while the PUT
    // was in flight, so newer edits are not discarded.
    setDraft(prev => (prev === saved ? null : prev))
    setSaved(true)
    setTimeout(() => setSaved(false), 3000)
    mutate(prev => (prev ? { ...prev, text: saved } : prev), { revalidate: false })
  }, [text, mutate])

  return (
    <Card>
      <CardHeader className="pb-3">
        <div className="flex items-center gap-2">
          <Bot className="h-4 w-4 text-muted-foreground" />
          <CardTitle className="text-base">Global Agent Instructions</CardTitle>
        </div>
        <CardDescription>
          Prepended to the agent's context on every run, for all repos. Set per-repo
          overrides from the Repos page. This never overwrites a repo's own AGENTS.md.
        </CardDescription>
      </CardHeader>
      <CardContent className="space-y-3">
        {error ? (
          <div className="flex items-center gap-2 text-sm text-red-600">
            <AlertTriangle className="h-4 w-4" />
            <span>Failed to load instructions: {error.message}. Editing is disabled to avoid overwriting.</span>
          </div>
        ) : (
          <textarea
            value={text}
            onChange={e => setDraft(e.target.value)}
            disabled={isLoading}
            placeholder="e.g. Prefer minimal diffs. Never edit generated files; fix the generator instead."
            className="w-full font-mono text-xs bg-muted/50 border rounded-md p-4 min-h-[160px] resize-y focus:outline-none focus:ring-2 focus:ring-primary/30 disabled:opacity-50"
            spellCheck={false}
          />
        )}
        <div className="flex items-center gap-3">
          <button
            onClick={handleSave}
            disabled={saving || !dirty || !!error}
            className="flex items-center gap-1.5 px-3 py-1.5 rounded-md text-xs font-medium bg-green-600 text-white hover:bg-green-700 disabled:opacity-40 disabled:pointer-events-none transition-colors"
          >
            <Save className="h-3.5 w-3.5" />
            {saving ? 'Saving...' : 'Save Instructions'}
          </button>
          {saved && !dirty && (
            <span className="text-xs text-green-600 flex items-center gap-1">
              <CheckCircle2 className="h-3.5 w-3.5" /> Saved
            </span>
          )}
          {dirty && !saveError && <span className="text-xs text-amber-600">Unsaved changes</span>}
          {saveError && (
            <span className="text-xs text-red-600 flex items-center gap-1">
              <AlertTriangle className="h-3.5 w-3.5" /> {saveError}
            </span>
          )}
        </div>
      </CardContent>
    </Card>
  )
}

type EditorMode = 'sections' | 'raw'

export default function ConfigPage() {
  const { user: currentUser } = useAuth()
  const isAdmin = currentUser?.role === ADMIN_ROLE
  const { data, isLoading, error, mutate } = useSWR<ConfigResponse>(isAdmin ? 'config' : null, fetchConfig)
  const [editContent, setEditContent] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)
  const [saveResult, setSaveResult] = useState<SaveResult | null>(null)
  const [revisionConflict, setRevisionConflict] = useState(false)
  const [mode, setMode] = useState<EditorMode>('sections')

  const savedContent = data?.content ?? ''
  const content = editContent ?? savedContent
  const hasUnsavedChanges = editContent !== null && editContent !== savedContent
  const writable = data?.writable ?? false
  const restartRequired = data?.restart_required ?? false

  const sections = useMemo(() => parseDocument(content), [content])
  const savedFields = useMemo(() => buildFieldMap(parseDocument(savedContent)), [savedContent])
  const secretPaths = useMemo(() => new Set(data?.secret_paths ?? []), [data?.secret_paths])
  const runtimeOverrides = useMemo(() => new Set(data?.runtime_overrides ?? []), [data?.runtime_overrides])

  const addableSections = useMemo(() => {
    const present = new Set(sections.map(section => section.key))
    return (data?.optional_sections ?? []).filter(section => !present.has(section.key))
  }, [data?.optional_sections, sections])

  useEffect(() => {
    if (saveResult?.ok) {
      const timeout = setTimeout(() => setSaveResult(null), 5000)
      return () => clearTimeout(timeout)
    }
  }, [saveResult])

  const reloadFromServer = useCallback(async () => {
    const fresh = await mutate()
    if (fresh) {
      setEditContent(null)
      setRevisionConflict(false)
    }
    return fresh
  }, [mutate])

  const handleSave = useCallback(async () => {
    if (editContent === null) return
    setSaving(true)
    setSaveResult(null)
    setRevisionConflict(false)
    try {
      const result = await saveConfig(editContent, data?.revision)
      setSaveResult({ ok: true, message: result.message })
      await mutate(
        current => current && { ...current, restart_required: result.restart_required },
        { revalidate: false },
      )
      await reloadFromServer()
    } catch (e: unknown) {
      if (e instanceof ApiError && e.code === REVISION_MISMATCH) setRevisionConflict(true)
      setSaveResult({ ok: false, message: e instanceof Error ? e.message : 'Unknown error' })
    } finally {
      setSaving(false)
    }
  }, [editContent, data?.revision, mutate, reloadFromServer])

  const handleDiscard = useCallback(() => {
    setEditContent(null)
    setSaveResult(null)
  }, [])

  const handleFieldChange = useCallback((sectionIndex: number, field: ParsedField, value: string) => {
    setEditContent(previous => replaceField(previous ?? savedContent, sectionIndex, field.path, value))
  }, [savedContent])

  const handleAddSection = useCallback((key: string) => {
    const section = addableSections.find(candidate => candidate.key === key)
    if (!section) return
    setEditContent(previous => appendTemplate(previous ?? savedContent, section.template))
  }, [addableSections, savedContent])

  const showRaw = useCallback(() => setMode('raw'), [])

  if (!isAdmin) {
    return (
      <div className="flex items-center gap-2 text-muted-foreground text-sm p-6">
        <ShieldAlert className="h-5 w-5" />
        <span>Access denied. You need administrator privileges to view configuration.</span>
      </div>
    )
  }

  return (
    <div className="space-y-6">
      <PageHeader title="Configuration" description="View and edit your claudear.toml config file" />

      <GlobalInstructionsCard />

      {restartRequired && (
        <div role="status" className="flex items-center gap-2 px-4 py-3 rounded-md text-sm bg-amber-50 border border-amber-200 text-amber-800">
          <RefreshCw className="h-4 w-4" />
          Restart required: saved changes to claudear.toml apply after claudear restarts. Restart the claudear
          process serving this console (and the daemon, if it runs separately).
        </div>
      )}

      {data && !writable && (
        <div role="alert" className="flex items-center gap-2 px-4 py-3 rounded-md text-sm bg-muted border text-muted-foreground">
          <Lock className="h-4 w-4" />
          Saving is disabled: claudear.toml is not writable by claudear (for example, a read-only Docker mount).
          Edit the file on the host and restart claudear.
        </div>
      )}

      <div className="flex items-center gap-3 flex-wrap">
        {hasUnsavedChanges && (
          <Badge variant="outline" className="text-xs text-amber-600 border-amber-300 bg-amber-50">
            Unsaved changes
          </Badge>
        )}

        <div className="flex-1" />

        {addableSections.length > 0 && (
          <label className="flex items-center gap-1.5 text-xs">
            <Plus className="h-3.5 w-3.5 text-muted-foreground" />
            <select
              aria-label="Add section"
              value=""
              onChange={e => handleAddSection(e.target.value)}
              className="h-7 rounded-md border bg-background px-2 text-xs"
            >
              <option value="">Add section...</option>
              {addableSections.map(section => (
                <option key={section.key} value={section.key}>
                  {section.key}
                </option>
              ))}
            </select>
          </label>
        )}

        <div className="flex items-center border rounded-md overflow-hidden text-xs">
          <button
            onClick={() => setMode('sections')}
            className={`px-3 py-1.5 transition-colors ${mode === 'sections' ? 'bg-primary text-primary-foreground' : 'bg-card hover:bg-muted'}`}
          >
            Sections
          </button>
          <button
            onClick={showRaw}
            className={`px-3 py-1.5 transition-colors ${mode === 'raw' ? 'bg-primary text-primary-foreground' : 'bg-card hover:bg-muted'}`}
          >
            Raw
          </button>
        </div>

        <button
          onClick={handleDiscard}
          disabled={!hasUnsavedChanges}
          className="flex items-center gap-1.5 px-3 py-1.5 rounded-md text-xs font-medium border hover:bg-muted transition-colors disabled:opacity-40 disabled:pointer-events-none"
        >
          <RotateCcw className="h-3.5 w-3.5" />
          Discard
        </button>
        <button
          onClick={handleSave}
          disabled={saving || !hasUnsavedChanges || !writable}
          title={writable ? undefined : 'claudear.toml is read-only'}
          className="flex items-center gap-1.5 px-3 py-1.5 rounded-md text-xs font-medium bg-green-600 text-white hover:bg-green-700 disabled:opacity-40 disabled:pointer-events-none transition-colors"
        >
          <Save className="h-3.5 w-3.5" />
          {saving ? 'Saving...' : 'Save'}
        </button>
      </div>

      <p className="flex items-start gap-1.5 text-xs text-muted-foreground">
        <Info className="h-3.5 w-3.5 mt-0.5 shrink-0" />
        Saving rewrites claudear.toml in normalised form; comments are removed and defaults written out.
        Values supplied by CLAUDEAR_* environment variables are not written to the file.
      </p>

      {saveResult && (
        <div role={saveResult.ok ? 'status' : 'alert'} className={`flex items-center gap-2 px-4 py-3 rounded-md text-sm ${
          saveResult.ok
            ? 'bg-green-50 border border-green-200 text-green-800'
            : 'bg-red-50 border border-red-200 text-red-800'
        }`}>
          {saveResult.ok ? <CheckCircle2 className="h-4 w-4" /> : <AlertTriangle className="h-4 w-4" />}
          {saveResult.message}
        </div>
      )}

      {revisionConflict && (
        <div className="flex items-center gap-3 px-4 py-3 rounded-md text-sm bg-amber-50 border border-amber-200 text-amber-800">
          <AlertTriangle className="h-4 w-4" />
          <span className="flex-1">
            claudear.toml changed on disk since it was loaded. Reload to get the latest version; your unsaved edits will be discarded.
          </span>
          <button
            type="button"
            onClick={() => void reloadFromServer()}
            className="px-3 py-1 rounded-md text-xs font-medium border border-amber-300 hover:bg-amber-100"
          >
            Reload
          </button>
        </div>
      )}

      {isLoading && (
        <CardStackSkeleton count={4} itemClassName="h-32" />
      )}

      {error && (
        <Card>
          <CardContent className="p-6">
            <div className="flex items-center gap-2 text-red-600">
              <AlertTriangle className="h-5 w-5" />
              <span className="text-sm">Failed to load config: {error.message}</span>
            </div>
          </CardContent>
        </Card>
      )}

      {!isLoading && !error && mode === 'sections' && (
        <div className="grid gap-4 md:grid-cols-2">
          {sections.map((section, sectionIndex) => (
            <SectionFormCard
              key={`${section.path || ROOT_KEY}-${sectionIndex}`}
              section={section}
              savedFields={savedFields}
              secretPaths={secretPaths}
              runtimeOverrides={runtimeOverrides}
              onChange={(field, value) => handleFieldChange(sectionIndex, field, value)}
              onEditRaw={showRaw}
            />
          ))}
        </div>
      )}

      {!isLoading && !error && mode === 'raw' && (
        <Card>
          <CardHeader className="pb-3">
            <div className="flex items-center gap-2">
              <FileText className="h-4 w-4 text-muted-foreground" />
              <CardTitle className="text-base">claudear.toml</CardTitle>
            </div>
          </CardHeader>
          <CardContent>
            <textarea
              aria-label="claudear.toml"
              value={content}
              onChange={e => setEditContent(e.target.value)}
              className="w-full font-mono text-xs bg-muted/50 border rounded-md p-4 min-h-[600px] resize-y focus:outline-none focus:ring-2 focus:ring-primary/30"
              spellCheck={false}
            />
          </CardContent>
        </Card>
      )}
    </div>
  )
}

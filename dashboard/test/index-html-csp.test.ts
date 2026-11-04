import { afterEach, describe, expect, test } from 'bun:test'
import { existsSync, readFileSync } from 'fs'
import { join } from 'path'

const dashboard = join(import.meta.dir, '..')
const html = readFileSync(join(dashboard, 'index.html'), 'utf-8')
const stylesheet = readFileSync(join(dashboard, 'src/index.css'), 'utf-8')
const page = new DOMParser().parseFromString(html, 'text/html')

function isSameOrigin(reference: string): boolean {
  return new URL(reference, 'http://dashboard.test/').origin === 'http://dashboard.test'
}

describe('index.html under the dashboard content security policy', () => {
  afterEach(() => {
    localStorage.clear()
    document.documentElement.classList.remove('dark')
  })

  test('has no inline scripts', () => {
    const scripts = [...page.querySelectorAll('script')]
    expect(scripts.length).toBeGreaterThan(0)
    for (const script of scripts) {
      expect(script.getAttribute('src')).toBeTruthy()
      expect(script.textContent?.trim()).toBe('')
    }
  })

  test('loads scripts and links only from its own origin', () => {
    const references = [
      ...[...page.querySelectorAll('script[src]')].map(script => script.getAttribute('src')!),
      ...[...page.querySelectorAll('link[href]')].map(link => link.getAttribute('href')!),
    ]
    for (const reference of references) {
      expect(isSameOrigin(reference), reference).toBe(true)
    }
  })

  test('stylesheet imports no remote fonts or styles', () => {
    expect(stylesheet).not.toMatch(/@import\s+(url\()?['"]?(https?:)?\/\//)
    expect(stylesheet).not.toMatch(/url\(\s*['"]?(https?:)?\/\//)
    expect(stylesheet).toContain('@fontsource/inter/')
    expect(stylesheet).toContain('@fontsource/jetbrains-mono/')
  })

  test('theme script is served from public and applies a saved dark theme', () => {
    const themePath = join(dashboard, 'public/theme.js')
    expect(page.querySelector('script[src="/theme.js"]')).not.toBeNull()
    expect(existsSync(themePath)).toBe(true)

    localStorage.setItem('theme', 'dark')
    new Function(readFileSync(themePath, 'utf-8'))()
    expect(document.documentElement.classList.contains('dark')).toBe(true)
  })

  test('theme script runs before the app bundle', () => {
    const sources = [...page.querySelectorAll('script')].map(script => script.getAttribute('src'))
    expect(sources.indexOf('/theme.js')).toBeLessThan(sources.indexOf('/src/main.tsx'))
    expect(page.querySelector('script[src="/theme.js"]')!.hasAttribute('async')).toBe(false)
    expect(page.querySelector('script[src="/theme.js"]')!.hasAttribute('defer')).toBe(false)
  })
})

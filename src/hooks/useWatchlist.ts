import { useCallback, useEffect, useState } from 'react'

const KEY = 'latein.watchlist'
const DEFAULT = ['AAPL', 'MSFT', 'NVDA', 'TSLA', 'AMZN']

function load(): string[] {
  try {
    const raw = localStorage.getItem(KEY)
    if (raw) return JSON.parse(raw) as string[]
  } catch {
    /* ignore malformed storage */
  }
  return DEFAULT
}

// Module-level store so every component shares a single watchlist instance.
let current = load()
const listeners = new Set<(v: string[]) => void>()

function broadcast(next: string[]): void {
  current = next
  try {
    localStorage.setItem(KEY, JSON.stringify(next))
  } catch {
    /* ignore quota / privacy-mode errors */
  }
  listeners.forEach((l) => l(current))
}

export function useWatchlist() {
  const [list, setList] = useState<string[]>(current)

  useEffect(() => {
    listeners.add(setList)
    setList(current)
    return () => {
      listeners.delete(setList)
    }
  }, [])

  const add = useCallback((symbol: string) => {
    const s = symbol.toUpperCase()
    if (current.includes(s)) return
    broadcast([...current, s])
  }, [])

  const remove = useCallback((symbol: string) => {
    broadcast(current.filter((s) => s !== symbol.toUpperCase()))
  }, [])

  return { list, add, remove }
}

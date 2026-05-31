import { useEffect, useMemo, useRef, useState } from 'react'
import { parseCommand } from '../command/parser'
import { lookupFunc, type PanelType } from '../command/registry'
import { useProvider } from '../data/DataProvider'
import { useWorkspace } from '../workspace/workspaceStore'
import { useWatchlist } from '../hooks/useWatchlist'

const PANEL_TITLES: Record<PanelType, string> = {
  quote: 'Quote',
  chart: 'Chart',
  news: 'News',
  watchlist: 'Watchlist',
  help: 'Help',
}

export function CommandBar() {
  const provider = useProvider()
  const { open } = useWorkspace()
  const { add } = useWatchlist()
  const [value, setValue] = useState('')
  const [error, setError] = useState<string | null>(null)
  const inputRef = useRef<HTMLInputElement>(null)

  // Focus on mount and whenever "/" is pressed outside the input.
  useEffect(() => {
    inputRef.current?.focus()
    const onKey = (e: KeyboardEvent) => {
      if (e.key === '/' && document.activeElement !== inputRef.current) {
        e.preventDefault()
        inputRef.current?.focus()
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [])

  const suggestions = useMemo(() => {
    const parsed = parseCommand(value)
    if (!parsed.symbol) return []
    return provider.search(parsed.symbol).slice(0, 6)
  }, [value, provider])

  function run(input: string) {
    const parsed = parseCommand(input)
    const fn = lookupFunc(parsed.func)
    if (!fn) {
      setError(`Unbekannte Funktion: ${parsed.func}`)
      return
    }
    if (fn.needsSymbol && !parsed.symbol) {
      setError(`${fn.code} benötigt einen Ticker`)
      return
    }
    if (parsed.symbol && !provider.getSecurity(parsed.symbol)) {
      setError(`Unbekanntes Wertpapier: ${parsed.symbol}`)
      return
    }

    setError(null)
    if (fn.action === 'add-watchlist' && parsed.symbol) {
      add(parsed.symbol)
      open({ type: 'watchlist', title: 'Watchlist' })
    } else if (fn.panel) {
      const sym = parsed.symbol
      const base = PANEL_TITLES[fn.panel]
      open({ type: fn.panel, symbol: sym, title: sym ? `${sym} ${base}` : base })
    }
    setValue('')
  }

  return (
    <div className="border-b border-terminal-border bg-terminal-bg px-2 py-1.5">
      <div className="flex items-center gap-2">
        <span className="font-bold text-terminal-accent">›</span>
        <input
          ref={inputRef}
          value={value}
          onChange={(e) => {
            setValue(e.target.value)
            setError(null)
          }}
          onKeyDown={(e) => {
            if (e.key === 'Enter') run(value)
          }}
          placeholder="z.B. AAPL GP  ·  MSFT N  ·  TSLA W  ·  HELP   (Enter = GO)"
          spellCheck={false}
          autoComplete="off"
          className="flex-1 bg-transparent uppercase tracking-wide text-terminal-text placeholder:normal-case placeholder:text-terminal-muted focus:outline-none"
        />
        <span className="hidden text-xs text-terminal-muted sm:inline">GO ⏎</span>
      </div>

      {error && <div className="mt-1 text-xs text-terminal-down">{error}</div>}

      {suggestions.length > 0 && (
        <div className="mt-1 flex flex-wrap gap-1">
          {suggestions.map((s) => (
            <button
              key={s.symbol}
              onClick={() => {
                setValue(`${s.symbol} `)
                inputRef.current?.focus()
              }}
              className="rounded border border-terminal-border px-1.5 py-0.5 text-xs text-terminal-muted hover:border-terminal-accent hover:text-terminal-text"
            >
              <span className="text-terminal-text">{s.symbol}</span> · {s.name}
            </button>
          ))}
        </div>
      )}
    </div>
  )
}
